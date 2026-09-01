use glass_core::{
    AxRole, FindElementsParams, Glass, MatchField, MatchTier, ScopeResolution, SemanticMatch,
    SemanticQuery, SemanticSelector, SemanticState,
};
use serde_json::{Value, json};

use crate::params::{FindElementsArgs, FindSelectorArgs};
use crate::tools::{OutContent, ToolOutput, ToolResult};

const DEFAULT_MAX_RESULTS: usize = 10;
pub(crate) const FIND_RESPONSE_MAX_BYTES: usize = 8_192;

#[derive(Clone)]
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
    context_stage: usize,
    field_stages: [usize; 3],
    fields_counted: [bool; 3],
    context_counted: bool,
    query: Option<String>,
}

#[derive(Default)]
struct FitCounters {
    omitted_by_budget: usize,
    fields_truncated: usize,
    contexts_truncated: usize,
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
        .map_err(|_| bounded_error("glass_find_elements failed"))?;

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
    let mut rendered = outcome
        .result
        .matches
        .iter()
        .map(|matched| render_match(matched, query.target.query()))
        .collect::<Vec<_>>();
    fit_output(&metadata, &mut rendered)
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

fn render_match(matched: &SemanticMatch, query: Option<&str>) -> RenderedMatch {
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
        context_stage: 0,
        field_stages: [0; 3],
        fields_counted: [false; 3],
        context_counted: false,
        query: query.map(str::to_owned),
    }
}

fn fit_output(metadata: &Metadata, rendered: &mut Vec<RenderedMatch>) -> ToolResult {
    let mut counters = FitCounters::default();
    let mut output = build_output(metadata, rendered, &counters);
    while text_bytes(&output) > FIND_RESPONSE_MAX_BYTES {
        if shorten_lowest_ranked_context(rendered, &mut counters) {
            output = build_output(metadata, rendered, &counters);
            continue;
        }
        if shorten_lowest_ranked_field(rendered, &mut counters) {
            output = build_output(metadata, rendered, &counters);
            continue;
        }
        if let Some(omitted) = rendered.pop() {
            counters.contexts_truncated -= usize::from(omitted.context_counted);
            counters.fields_truncated -= omitted
                .fields_counted
                .into_iter()
                .filter(|counted| *counted)
                .count();
            counters.omitted_by_budget += 1;
            output = build_output(metadata, rendered, &counters);
            continue;
        }
        return Err(bounded_error(
            "glass_find_elements could not fit mandatory metadata within 8192 bytes",
        ));
    }
    Ok(output)
}

fn build_output(
    metadata: &Metadata,
    rendered: &[RenderedMatch],
    counters: &FitCounters,
) -> ToolOutput {
    let matches = rendered.iter().map(match_json).collect::<Vec<_>>();
    let mut result = json!({
        "matched": metadata.matched,
        "timed_out": metadata.timed_out,
        "elapsed_ms": metadata.elapsed_ms,
        "scope_status": metadata.scope_status,
        "matches_in_walk": metadata.matches_in_walk,
        "returned": rendered.len(),
        "omitted_by_max_results": metadata.omitted_by_max_results,
        "omitted_by_budget": counters.omitted_by_budget,
        "fields_truncated": counters.fields_truncated,
        "contexts_truncated": counters.contexts_truncated,
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
        vec![OutContent::Text(crate::untrusted::wrap_untrusted(&body))],
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

fn shorten_lowest_ranked_context(
    rendered: &mut [RenderedMatch],
    counters: &mut FitCounters,
) -> bool {
    for matched in rendered.iter_mut().rev() {
        if matched.context_stage >= 3 {
            continue;
        }
        matched.context_stage += 1;
        let limit = [usize::MAX, 512, 128, 0][matched.context_stage];
        let shortened = truncate_text(&matched.context, limit, None);
        let changed = shortened != matched.context;
        matched.context = shortened;
        if changed && !matched.context_counted {
            matched.context_counted = true;
            counters.contexts_truncated += 1;
        }
        return true;
    }
    false
}

fn shorten_lowest_ranked_field(rendered: &mut [RenderedMatch], counters: &mut FitCounters) -> bool {
    for matched in rendered.iter_mut().rev() {
        for index in (0..3).rev() {
            if matched.field_stages[index] >= 3 || field(matched, index).is_none() {
                continue;
            }
            matched.field_stages[index] += 1;
            let limit = [usize::MAX, 256, 96, 0][matched.field_stages[index]];
            let preserve = (matched.matched_field == Some(field_name(index)))
                .then_some(matched.query.as_deref())
                .flatten();
            let shortened = truncate_text(
                field(matched, index).as_deref().unwrap_or(""),
                limit,
                preserve,
            );
            let changed = field(matched, index).as_deref() != Some(shortened.as_str());
            *field_mut(matched, index) = Some(shortened);
            if changed && !matched.fields_counted[index] {
                matched.fields_counted[index] = true;
                counters.fields_truncated += 1;
            }
            return true;
        }
    }
    false
}

fn field(matched: &RenderedMatch, index: usize) -> &Option<String> {
    match index {
        0 => &matched.name,
        1 => &matched.description,
        _ => &matched.value,
    }
}

fn field_mut(matched: &mut RenderedMatch, index: usize) -> &mut Option<String> {
    match index {
        0 => &mut matched.name,
        1 => &mut matched.description,
        _ => &mut matched.value,
    }
}

fn field_name(index: usize) -> &'static str {
    match index {
        0 => "name",
        1 => "description",
        _ => "value",
    }
}

fn truncate_text(text: &str, max_bytes: usize, preserve: Option<&str>) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes == 0 {
        return String::new();
    }
    let ellipsis = "…";
    if max_bytes < ellipsis.len() {
        return String::new();
    }
    let matched = preserve
        .filter(|needle| !needle.is_empty())
        .and_then(|needle| case_insensitive_match_range(text, needle));
    let leading_ellipsis = matched.as_ref().is_some_and(|range| range.start > 0);
    let trailing_ellipsis = matched.as_ref().is_none_or(|range| range.end < text.len());
    let content_bytes = max_bytes
        .saturating_sub(usize::from(leading_ellipsis) * ellipsis.len())
        .saturating_sub(usize::from(trailing_ellipsis) * ellipsis.len());
    let (start, end) = if let Some(matched) = matched {
        let retained_match_end =
            floor_char_boundary(text, (matched.start + content_bytes).min(matched.end));
        let retained_match_len = retained_match_end - matched.start;
        let surrounding = content_bytes.saturating_sub(retained_match_len);
        let start = floor_char_boundary(text, matched.start.saturating_sub(surrounding / 3));
        let end = floor_char_boundary(text, (start + content_bytes).min(text.len()));
        if end < retained_match_end {
            let start = floor_char_boundary(text, retained_match_end.saturating_sub(content_bytes));
            (start, retained_match_end)
        } else {
            (start, end)
        }
    } else {
        (0, floor_char_boundary(text, content_bytes.min(text.len())))
    };
    let mut output = String::new();
    if start > 0 {
        output.push('…');
    }
    output.push_str(&text[start..end]);
    if end < text.len() {
        output.push('…');
    }
    while output.len() > max_bytes {
        let end = floor_char_boundary(&output, output.len().saturating_sub(1));
        output.truncate(end);
    }
    output
}

fn case_insensitive_match_range(text: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    let folded_needle = needle.to_lowercase();
    let mut folded_text = String::new();
    let mut spans = Vec::new();
    for (start, character) in text.char_indices() {
        let folded_start = folded_text.len();
        folded_text.extend(character.to_lowercase());
        spans.push((
            folded_start,
            folded_text.len(),
            start,
            start + character.len_utf8(),
        ));
    }

    let folded_start = folded_text.find(&folded_needle)?;
    let folded_end = folded_start + folded_needle.len();
    let start = spans
        .iter()
        .find(|(_, end, _, _)| *end > folded_start)
        .map(|(_, _, start, _)| *start)?;
    let end = spans
        .iter()
        .find(|(start, end, _, _)| *start < folded_end && *end >= folded_end)
        .map(|(_, _, _, end)| *end)?;
    Some(start..end)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn text_bytes(output: &ToolOutput) -> usize {
    output
        .0
        .iter()
        .map(|content| match content {
            OutContent::Text(text) => text.len(),
            OutContent::Image(_) => 0,
        })
        .sum()
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
    truncate_text(safe, FIND_RESPONSE_MAX_BYTES, None)
}

#[cfg(test)]
#[allow(clippy::needless_as_bytes)]
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
    fn truncation_preserves_a_long_match_near_the_end() {
        let matched = "target-region-".repeat(5);
        let text = format!("{}{}{}", "before-".repeat(40), matched, "after-".repeat(40));

        let truncated = truncate_text(&text, 96, Some(&matched));

        assert!(truncated.contains(&matched), "{truncated:?}");
        assert!(truncated.len() <= 96);
    }

    #[test]
    fn truncation_preserves_match_offsets_across_unicode_lowercase_expansion() {
        let matched = "target-region-".repeat(5);
        let text = format!("{}{}{}", "İ".repeat(80), matched, "after-".repeat(40));

        let truncated = truncate_text(&text, 96, Some(&matched.to_uppercase()));

        assert!(truncated.contains(&matched), "{truncated:?}");
        assert!(truncated.len() <= 96);
        assert!(truncated.is_char_boundary(truncated.len()));
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
    fn find_success_and_error_responses_never_exceed_8192_bytes() {
        let mut tree = fake_tree();
        for index in 0..20 {
            let mut node = tree.root.children[0].clone();
            node.name = Some(format!("Save {index} {}", "x".repeat(20_000)));
            node.description = Some("y".repeat(20_000));
            node.value = Some("z".repeat(20_000));
            tree.root.children.push(node);
        }
        tree.assign_ids();
        let mut glass = started_a11y_with(tree);
        let output = find_elements(
            &mut glass,
            &FindElementsArgs {
                query: Some("save".into()),
                role: None,
                states: None,
                within: None,
                max_results: Some(20),
                max_nodes: Some(0),
                timeout_ms: None,
            },
        )
        .unwrap();
        assert!(text_bytes(&output) <= FIND_RESPONSE_MAX_BYTES);
        assert_valid_wrapped_match_json(&output);
        let error = bounded_error("e".repeat(20_000));
        assert!(error.as_bytes().len() <= FIND_RESPONSE_MAX_BYTES);
    }
}
