use glass_core::{
    AxRole, FindElementsParams, Glass, MatchField, MatchTier, ScopeResolution, SemanticMatch,
    SemanticQuery, SemanticSelector, SemanticState,
};
use serde_json::{Value, json};

use crate::params::{FindElementsArgs, FindSelectorArgs};
use crate::tools::{OutContent, ToolOutput, ToolResult};

const DEFAULT_MAX_RESULTS: usize = 10;

struct RenderedMatch {
    id: u32,
    role: String,
    name: Option<String>,
    description: Option<String>,
    value: Option<String>,
    bounds: Option<Value>,
    states: Vec<&'static str>,
    matched_field: Option<&'static str>,
    match_tier: &'static str,
    context: String,
}

struct Metadata {
    matched: bool,
    timed_out: bool,
    elapsed_ms: u64,
    scope_status: &'static str,
    resolved_scope_id: Option<u32>,
    matches_in_walk: usize,
    omitted_by_max_results: usize,
    search_complete: bool,
    tree_truncated: bool,
    unreadable_subtrees: usize,
    unexposed_placeholders: usize,
}

pub fn find_elements(glass: &mut Glass, a: &FindElementsArgs) -> ToolResult {
    let max_results = a.max_results.unwrap_or(DEFAULT_MAX_RESULTS as u32);
    if !(1..=20).contains(&max_results) {
        return Err(bounded_error("max_results must be between 1 and 20"));
    }

    let target = selector(a.query.clone(), a.role.as_deref(), a.states.as_deref())?;
    let within = a.within.as_ref().map(selector_args).transpose()?;
    let query = SemanticQuery::new(target, within, max_results as usize)
        .map_err(|error| bounded_error(error.to_string()))?;
    let outcome = glass
        .find_elements(&FindElementsParams {
            query: query.clone(),
            max_nodes: a.max_nodes.map(|value| value as usize),
            timeout_ms: a.timeout_ms.unwrap_or(0),
        })
        .map_err(safe_operational_error)?;

    if let ScopeResolution::Ambiguous { observed } = outcome.result.scope {
        return Err(bounded_error(format!(
            "semantic scope matched {observed} elements; refine `within` so it matches exactly one"
        )));
    }

    let (scope_status, resolved_scope_id) = match outcome.result.scope {
        ScopeResolution::Unscoped => ("unscoped", None),
        ScopeResolution::NotFound => ("not_found", None),
        ScopeResolution::Resolved(id) => ("resolved", Some(id.0)),
        ScopeResolution::Ambiguous { .. } => unreachable!(),
    };
    let metadata = Metadata {
        matched: outcome.matched,
        timed_out: outcome.timed_out,
        elapsed_ms: outcome.elapsed_ms,
        scope_status,
        resolved_scope_id,
        matches_in_walk: outcome.result.matches_in_walk,
        omitted_by_max_results: outcome.result.omitted_by_max_results,
        search_complete: outcome.result.search_complete,
        tree_truncated: outcome.result.tree_truncated.is_some(),
        unreadable_subtrees: outcome.result.unreadable_subtrees,
        unexposed_placeholders: outcome.result.unexposed_placeholders,
    };
    let rendered = outcome
        .result
        .matches
        .iter()
        .map(render_match)
        .collect::<Vec<_>>();
    Ok(build_output(&metadata, &rendered))
}

fn selector_args(args: &FindSelectorArgs) -> Result<SemanticSelector, String> {
    selector(
        args.query.clone(),
        args.role.as_deref(),
        args.states.as_deref(),
    )
}

fn selector(
    query: Option<String>,
    role: Option<&str>,
    states: Option<&[String]>,
) -> Result<SemanticSelector, String> {
    let role = role
        .map(|name| {
            AxRole::from_name(name)
                .ok_or_else(|| bounded_error("unknown role; use a normalized accessibility role"))
        })
        .transpose()?;
    let states = states
        .unwrap_or_default()
        .iter()
        .map(|name| {
            SemanticState::from_name(name).ok_or_else(|| {
                bounded_error("unknown state; use a normalized semantic state predicate")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    SemanticSelector::new(query, role, states).map_err(|error| bounded_error(error.to_string()))
}

fn render_match(matched: &SemanticMatch) -> RenderedMatch {
    RenderedMatch {
        id: matched.element.id.0,
        role: format!("{:?}", matched.element.role),
        name: matched.element.name.clone(),
        description: matched.element.description.clone(),
        value: if matched.element.states.secure {
            None
        } else {
            matched.element.value.clone()
        },
        bounds: matched.element.bounds.map(|bounds| {
            json!({
                "x": bounds.x,
                "y": bounds.y,
                "width": bounds.width,
                "height": bounds.height,
            })
        }),
        states: matched.element.states.active(),
        matched_field: matched.field.map(match_field_name),
        match_tier: match_tier_name(matched.tier),
        context: matched.context.clone(),
    }
}

fn build_output(metadata: &Metadata, rendered: &[RenderedMatch]) -> ToolOutput {
    let matches = rendered.iter().map(match_json).collect::<Vec<_>>();
    let mut result = json!({
        "matched": metadata.matched,
        "timed_out": metadata.timed_out,
        "elapsed_ms": metadata.elapsed_ms,
        "scope_status": metadata.scope_status,
        "matches_in_walk": metadata.matches_in_walk,
        "returned": rendered.len(),
        "omitted_by_max_results": metadata.omitted_by_max_results,
        "omitted_by_budget": 0,
        "fields_truncated": 0,
        "contexts_truncated": 0,
        "search_complete": metadata.search_complete,
        "tree_truncated": metadata.tree_truncated,
        "unreadable_subtrees": metadata.unreadable_subtrees,
        "unexposed_placeholders": metadata.unexposed_placeholders,
    });
    if let Some(id) = metadata.resolved_scope_id {
        result["resolved_scope_id"] = json!(id);
    }
    let body = json!({ "matches": matches }).to_string();
    ToolOutput::result_with(
        "glass_find_elements",
        result,
        vec![OutContent::untrusted_observation(&body)],
    )
}

fn match_json(matched: &RenderedMatch) -> Value {
    json!({
        "id": matched.id,
        "role": matched.role,
        "name": matched.name,
        "description": matched.description,
        "value": matched.value,
        "bounds": matched.bounds,
        "states": matched.states,
        "matched_field": matched.matched_field,
        "match_tier": matched.match_tier,
        "context": matched.context,
    })
}

fn match_field_name(field: MatchField) -> &'static str {
    match field {
        MatchField::Name => "name",
        MatchField::Description => "description",
        MatchField::Value => "value",
    }
}

fn match_tier_name(tier: MatchTier) -> &'static str {
    match tier {
        MatchTier::ExactName => "exact_name",
        MatchTier::NameSubstring => "name_substring",
        MatchTier::DescriptionSubstring => "description_substring",
        MatchTier::ValueSubstring => "value_substring",
        MatchTier::FilterOnly => "filter_only",
    }
}

pub(crate) fn bounded_error(error: impl AsRef<str>) -> String {
    let error = error.as_ref();
    let safe = if error.starts_with("specify query, role, and/or states")
        || error.starts_with("query must not be empty")
        || error.starts_with("contradictory states:")
        || error.starts_with("max_results must be between 1 and 20")
        || error.starts_with("unknown role;")
        || error.starts_with("unknown state;")
        || error.starts_with("semantic scope matched ")
        || error.starts_with("glass_find_elements ")
    {
        error
    } else {
        "glass_find_elements failed"
    };
    safe.to_owned()
}

fn safe_operational_error(error: glass_core::GlassError) -> String {
    let (category, guidance) = match error.cause() {
        glass_core::GlassError::NoActiveSession => (
            "no_active_session",
            "Call glass_start before glass_find_elements.",
        ),
        glass_core::GlassError::AxUnsupported
        | glass_core::GlassError::AccessibilityUnavailable(_) => (
            "unsupported_accessibility",
            "Use glass_screenshot for pixel-based inspection, or start with accessibility enabled.",
        ),
        glass_core::GlassError::PermissionDenied { .. } => (
            "permission_denied",
            "Grant the platform accessibility permission, then retry.",
        ),
        glass_core::GlassError::CaptureFailed(_)
        | glass_core::GlassError::Backend(_)
        | glass_core::GlassError::ToolFailed { .. }
        | glass_core::GlassError::Bounded { .. }
        | glass_core::GlassError::Io(_) => (
            "transport_failure",
            "Retry after checking the backend connection and app session.",
        ),
        _ => (
            "other",
            "Check the active session and accessibility setup before retrying.",
        ),
    };
    bounded_error(format!(
        "glass_find_elements failed [{category}]. {guidance}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;

    fn assert_valid_wrapped_match_json(output: &ToolOutput) {
        let OutContent::Text(text) = &output.0[1] else {
            panic!("text block")
        };
        let after_note = text.split_once('\n').unwrap().1;
        let after_open = after_note.split_once('\n').unwrap().1;
        let body = after_open.rsplit_once('\n').unwrap().0;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(value["matches"].is_array());
    }

    fn glass_with_twenty_long_matches() -> Glass {
        let mut tree = fake_tree();
        tree.root.children.truncate(1);
        tree.root.children[0].name = Some(format!("Save 0 {}", "x".repeat(1_000)));
        tree.root.children[0].description = Some("y".repeat(1_000));
        for index in 1..20 {
            let mut node = tree.root.children[0].clone();
            node.name = Some(format!("Save {index} {}", "x".repeat(1_000)));
            tree.root.children.push(node);
        }
        tree.assign_ids();
        started_a11y_with(tree)
    }

    fn args_with_max_results(max_results: u32) -> FindElementsArgs {
        FindElementsArgs {
            query: Some("save".into()),
            role: None,
            states: None,
            within: None,
            max_results: Some(max_results),
            max_nodes: Some(0),
            timeout_ms: None,
        }
    }

    #[test]
    fn find_keeps_all_selected_records_and_zeroes_legacy_budget_counters() {
        let mut glass = glass_with_twenty_long_matches();
        let output = find_elements(&mut glass, &args_with_max_results(20)).unwrap();
        let envelope = envelope_at(&output, 0);
        assert_eq!(envelope["result"]["returned"], 20);
        assert_eq!(envelope["result"]["omitted_by_budget"], 0);
        assert_eq!(envelope["result"]["fields_truncated"], 0);
        assert_eq!(envelope["result"]["contexts_truncated"], 0);
        assert!(
            output.text_bytes() > 8_192,
            "central server policy owns this bound"
        );
    }

    #[test]
    fn find_rejects_an_empty_selector_before_snapshotting() {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut glass = started_counted_a11y(reads.clone(), fake_tree());
        let error = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: None,
                role: None,
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("specify query, role, and/or states"));
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn find_rejects_whitespace_only_target_and_within_queries_before_snapshotting() {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut glass = started_counted_a11y(reads.clone(), fake_tree());
        let target_error = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some(" \t ".into()),
                role: None,
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(target_error.contains("query must not be empty"));

        let within_error = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: None,
                states: None,
                within: Some(FindSelectorArgs {
                    query: Some(" \n ".into()),
                    role: None,
                    states: None,
                }),
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(within_error.contains("query must not be empty"));
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn find_matches_queries_with_surrounding_whitespace() {
        let mut glass = started_a11y_with(fake_tree());
        let output = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("  save  ".into()),
                role: None,
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap();
        assert_eq!(envelope_at(&output, 0)["result"]["matched"], json!(true));
    }

    #[test]
    fn find_rejects_unknown_contradictory_and_out_of_range_selectors() {
        let mut glass = started_a11y_with(fake_tree());
        let unknown_role = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: Some("NotARole".into()),
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(unknown_role.contains("unknown role"));
        let contradictory = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: None,
                role: Some("Button".into()),
                states: Some(vec!["enabled".into(), "disabled".into()]),
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(contradictory.contains("contradictory states"));
        let out_of_range = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: None,
                states: None,
                within: None,
                max_results: Some(21),
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap_err();
        assert!(out_of_range.contains("between 1 and 20"));
    }

    #[test]
    fn find_returns_trusted_counts_and_one_untrusted_match_array() {
        let mut tree = fake_tree();
        tree.root.children[0].name = Some("Save account".into());
        let mut glass = started_a11y_with(tree);
        let output = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: Some("Button".into()),
                states: Some(vec!["enabled".into()]),
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap();
        let envelope = envelope_at(&output, 0);
        assert_eq!(envelope["tool"], serde_json::json!("glass_find_elements"));
        let result = &envelope["result"];
        assert_eq!(result["matched"], serde_json::json!(true));
        assert!(result.get("matches").is_none());
        assert_eq!(output.0.len(), 2);
        let OutContent::Text(untrusted) = &output.0[1] else {
            panic!("text block")
        };
        assert!(untrusted.starts_with(crate::untrusted::NOTE));
        assert!(untrusted.contains("\"name\":\"Save account\""));
    }

    #[test]
    fn find_never_searches_or_returns_secure_values() {
        let mut tree = fake_tree();
        tree.root.children[0].states.secure = true;
        tree.root.children[0].states.editable = true;
        tree.root.children[0].value = Some("needle-secret".into());
        let mut glass = started_a11y_with(tree);
        let output = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("needle".into()),
                role: None,
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap();
        let envelope = envelope_at(&output, 0);
        assert_eq!(envelope["tool"], serde_json::json!("glass_find_elements"));
        assert_eq!(envelope["result"]["matched"], serde_json::json!(false));
        assert!(!format!("{output:?}").contains("needle-secret"));
    }

    #[test]
    fn find_redacts_secure_neighbor_values_from_successful_match_context() {
        let mut tree = fake_tree();
        let mut secure_neighbor = tree.root.children[0].clone();
        secure_neighbor.name = Some("Password".into());
        secure_neighbor.states.secure = true;
        secure_neighbor.states.editable = true;
        secure_neighbor.value = Some("context-secret-sentinel".into());
        tree.root.children.push(secure_neighbor);
        tree.assign_ids();
        let mut glass = started_a11y_with(tree);

        let output = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: Some("Button".into()),
                states: None,
                within: None,
                max_results: None,
                max_nodes: None,
                timeout_ms: None,
            },
        )
        .unwrap();

        assert_eq!(envelope_at(&output, 0)["result"]["matched"], json!(true));
        let OutContent::Text(untrusted) = &output.0[1] else {
            panic!("text block")
        };
        assert!(untrusted.contains("\"name\":\"Save\""));
        assert!(!format!("{output:?}").contains("context-secret-sentinel"));
    }

    #[test]
    fn permission_denial_keeps_a_safe_actionable_category() {
        let error = safe_operational_error(glass_core::GlassError::PermissionDenied {
            which: "accessibility".into(),
            remedy: "backend-controlled-secret".into(),
        });
        assert!(error.contains("permission_denied"), "{error}");
        assert!(error.contains("Grant the platform accessibility permission"));
        assert!(!error.contains("backend-controlled-secret"));
    }
}
