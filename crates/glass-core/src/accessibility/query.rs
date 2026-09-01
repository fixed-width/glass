use crate::accessibility::{
    AxNode, AxNodeId, AxRole, AxStates, AxTree, ElementCondition, ElementInfo,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SemanticState {
    Enabled,
    Disabled,
    Checked,
    Unchecked,
    Selected,
    Unselected,
    Expanded,
    Collapsed,
    Focused,
    Visible,
    Hidden,
}

impl SemanticState {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "enabled" => Self::Enabled,
            "disabled" => Self::Disabled,
            "checked" => Self::Checked,
            "unchecked" => Self::Unchecked,
            "selected" => Self::Selected,
            "unselected" => Self::Unselected,
            "expanded" => Self::Expanded,
            "collapsed" => Self::Collapsed,
            "focused" => Self::Focused,
            "visible" => Self::Visible,
            "hidden" => Self::Hidden,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Checked => "checked",
            Self::Unchecked => "unchecked",
            Self::Selected => "selected",
            Self::Unselected => "unselected",
            Self::Expanded => "expanded",
            Self::Collapsed => "collapsed",
            Self::Focused => "focused",
            Self::Visible => "visible",
            Self::Hidden => "hidden",
        }
    }

    pub fn matches(self, states: &AxStates) -> bool {
        let condition = match self {
            Self::Enabled => ElementCondition::Enabled,
            Self::Disabled => ElementCondition::Disabled,
            Self::Checked => ElementCondition::Checked,
            Self::Unchecked => ElementCondition::Unchecked,
            Self::Selected => ElementCondition::Selected,
            Self::Unselected => ElementCondition::Unselected,
            Self::Expanded => ElementCondition::Expanded,
            Self::Collapsed => ElementCondition::Collapsed,
            Self::Focused => ElementCondition::Focused,
            Self::Visible => ElementCondition::Visible,
            Self::Hidden => ElementCondition::Hidden,
        };
        (condition.state_pred())(states)
    }

    fn opposite(self) -> Option<Self> {
        Some(match self {
            Self::Enabled => Self::Disabled,
            Self::Disabled => Self::Enabled,
            Self::Checked => Self::Unchecked,
            Self::Unchecked => Self::Checked,
            Self::Selected => Self::Unselected,
            Self::Unselected => Self::Selected,
            Self::Expanded => Self::Collapsed,
            Self::Collapsed => Self::Expanded,
            Self::Visible => Self::Hidden,
            Self::Hidden => Self::Visible,
            Self::Focused => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SemanticQueryError {
    #[error("specify query, role, and/or states")]
    EmptySelector,
    #[error("query must not be empty")]
    EmptyQuery,
    #[error("contradictory states: {first} and {second}")]
    ContradictoryStates {
        first: &'static str,
        second: &'static str,
    },
    #[error("max_results must be between 1 and 20")]
    InvalidMaxResults,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticSelector {
    query: Option<String>,
    folded_query: Option<String>,
    role: Option<AxRole>,
    states: Vec<SemanticState>,
}

impl SemanticSelector {
    pub fn new(
        query: Option<String>,
        role: Option<AxRole>,
        states: Vec<SemanticState>,
    ) -> Result<Self, SemanticQueryError> {
        let query = match query {
            Some(query) if query.is_empty() => return Err(SemanticQueryError::EmptyQuery),
            value => value,
        };
        if query.is_none() && role.is_none() && states.is_empty() {
            return Err(SemanticQueryError::EmptySelector);
        }

        let mut deduplicated = Vec::with_capacity(states.len());
        for state in states {
            if deduplicated.contains(&state) {
                continue;
            }
            if let Some(opposite) = state.opposite()
                && deduplicated.contains(&opposite)
            {
                return Err(SemanticQueryError::ContradictoryStates {
                    first: opposite.as_str(),
                    second: state.as_str(),
                });
            }
            deduplicated.push(state);
        }

        let folded_query = query.as_deref().map(str::to_lowercase);
        Ok(Self {
            query,
            folded_query,
            role,
            states: deduplicated,
        })
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub fn role(&self) -> Option<AxRole> {
        self.role
    }

    pub fn states(&self) -> &[SemanticState] {
        &self.states
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticQuery {
    pub target: SemanticSelector,
    pub within: Option<SemanticSelector>,
    pub max_results: usize,
}

impl SemanticQuery {
    pub fn new(
        target: SemanticSelector,
        within: Option<SemanticSelector>,
        max_results: usize,
    ) -> Result<Self, SemanticQueryError> {
        if !(1..=20).contains(&max_results) {
            return Err(SemanticQueryError::InvalidMaxResults);
        }
        Ok(Self {
            target,
            within,
            max_results,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeResolution {
    Unscoped,
    NotFound,
    Resolved(AxNodeId),
    Ambiguous { observed: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchField {
    Name,
    Description,
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    ExactName,
    NameSubstring,
    DescriptionSubstring,
    ValueSubstring,
    FilterOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticMatch {
    pub element: ElementInfo,
    pub field: Option<MatchField>,
    pub tier: MatchTier,
    pub context: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticQueryResult {
    pub scope: ScopeResolution,
    pub matches_in_walk: usize,
    pub matches: Vec<SemanticMatch>,
    pub omitted_by_max_results: usize,
    pub search_complete: bool,
    pub tree_truncated: Option<crate::Truncation>,
    pub unreadable_subtrees: usize,
    pub unexposed_placeholders: usize,
}

impl AxTree {
    pub fn semantic_query(&self, query: &SemanticQuery) -> SemanticQueryResult {
        query_tree(self, query)
    }
}

fn tier_for(node: &AxNode, selector: &SemanticSelector) -> Option<(MatchTier, Option<MatchField>)> {
    if selector.role().is_some_and(|role| node.role != role) {
        return None;
    }
    if !selector
        .states()
        .iter()
        .all(|state| state.matches(&node.states))
    {
        return None;
    }
    let Some(needle) = selector.folded_query.as_deref() else {
        return Some((MatchTier::FilterOnly, None));
    };
    let folded_name = node.name.as_deref().map(str::to_lowercase);
    if folded_name.as_deref() == Some(needle) {
        return Some((MatchTier::ExactName, Some(MatchField::Name)));
    }
    if folded_name
        .as_deref()
        .is_some_and(|value| value.contains(needle))
    {
        return Some((MatchTier::NameSubstring, Some(MatchField::Name)));
    }
    if node
        .description
        .as_deref()
        .map(str::to_lowercase)
        .as_deref()
        .is_some_and(|value| value.contains(needle))
    {
        return Some((
            MatchTier::DescriptionSubstring,
            Some(MatchField::Description),
        ));
    }
    if !node.states.secure
        && node
            .value
            .as_deref()
            .map(str::to_lowercase)
            .as_deref()
            .is_some_and(|value| value.contains(needle))
    {
        return Some((MatchTier::ValueSubstring, Some(MatchField::Value)));
    }
    None
}

fn walk<'a>(node: &'a AxNode, nodes: &mut Vec<&'a AxNode>) {
    nodes.push(node);
    for child in &node.children {
        walk(child, nodes);
    }
}

fn query_tree(tree: &AxTree, query: &SemanticQuery) -> SemanticQueryResult {
    let mut all_nodes = Vec::with_capacity(tree.count);
    walk(&tree.root, &mut all_nodes);

    let scope = if let Some(selector) = &query.within {
        let observed_scopes: Vec<_> = all_nodes
            .iter()
            .copied()
            .filter(|node| tier_for(node, selector).is_some())
            .collect();
        match observed_scopes.as_slice() {
            [] => ScopeResolution::NotFound,
            [node] => ScopeResolution::Resolved(node.id),
            nodes => ScopeResolution::Ambiguous {
                observed: nodes.len(),
            },
        }
    } else {
        ScopeResolution::Unscoped
    };

    let search_root = match scope {
        ScopeResolution::Unscoped => Some(&tree.root),
        ScopeResolution::Resolved(id) => tree.find(id),
        ScopeResolution::NotFound | ScopeResolution::Ambiguous { .. } => None,
    };
    let mut candidates = Vec::new();
    if let Some(search_root) = search_root {
        let mut search_nodes = Vec::new();
        walk(search_root, &mut search_nodes);
        for (preorder_index, node) in search_nodes.into_iter().enumerate() {
            if let Some((tier, field)) = tier_for(node, &query.target) {
                candidates.push((tier, preorder_index, field, node));
            }
        }
    }
    candidates.sort_by_key(|(tier, preorder_index, _, _)| (*tier, *preorder_index));

    let matches_in_walk = candidates.len();
    let matches = candidates
        .into_iter()
        .take(query.max_results)
        .map(|(tier, _, field, node)| {
            let mut element = ElementInfo::from_node(node);
            if element.states.secure {
                element.value = None;
            }
            SemanticMatch {
                element,
                field,
                tier,
                context: context_for(tree, node, scope),
            }
        })
        .collect();

    SemanticQueryResult {
        scope,
        matches_in_walk,
        matches,
        omitted_by_max_results: matches_in_walk.saturating_sub(query.max_results),
        search_complete: tree.can_prove_absence(),
        tree_truncated: tree.truncated,
        unreadable_subtrees: tree.unreadable,
        unexposed_placeholders: tree.unexposed,
    }
}

fn context_for(tree: &AxTree, node: &AxNode, scope: ScopeResolution) -> String {
    let Some(path) = tree.path_to(node.id) else {
        return String::new();
    };
    let scope_index = match scope {
        ScopeResolution::Resolved(id) => path
            .iter()
            .position(|candidate| candidate.id == id)
            .unwrap_or(0),
        _ => 0,
    };
    let ancestors = &path[scope_index..path.len().saturating_sub(1)];
    let retained_from = ancestors.len().saturating_sub(4);

    let mut output = String::from("ancestors:\n");
    if retained_from > 0 {
        output.push_str("  … higher ancestors omitted\n");
    }
    for ancestor in &ancestors[retained_from..] {
        crate::outline::write_line(ancestor, 1, &mut output, true);
    }

    output.push_str("neighbors:\n");
    if path.len() >= 2 {
        let parent = path[path.len() - 2];
        if let Some(index) = parent
            .children
            .iter()
            .position(|candidate| candidate.id == node.id)
        {
            if let Some(previous) = index.checked_sub(1).and_then(|i| parent.children.get(i)) {
                crate::outline::write_line(previous, 1, &mut output, true);
            }
            if let Some(next) = parent.children.get(index + 1) {
                crate::outline::write_line(next, 1, &mut output, true);
            }
        }
    }

    output.push_str("children:\n");
    for child in node.children.iter().take(3) {
        crate::outline::write_line(child, 1, &mut output, true);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AxNode, AxNodeId, AxRect, AxStates, AxTree};

    fn node(role: AxRole, name: Option<&str>) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role,
            raw_role: String::new(),
            name: name.map(str::to_string),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: Vec::new(),
        }
    }

    fn tree(children: Vec<AxNode>) -> AxTree {
        let mut root = node(AxRole::Window, Some("App"));
        root.children = children;
        let mut tree = AxTree::new(root);
        tree.assign_ids();
        tree
    }

    #[test]
    fn semantic_query_searches_all_public_text_case_insensitively() {
        let mut by_name = node(AxRole::Button, Some("Save Account"));
        by_name.bounds = Some(AxRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        let mut by_description = node(AxRole::Button, None);
        by_description.description = Some("SAVE draft".into());
        let mut by_value = node(AxRole::TextField, Some("Status"));
        by_value.value = Some("saved successfully".into());
        let tree = tree(vec![by_name, by_description, by_value]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("SaVe".into()), None, Vec::new()).unwrap(),
            None,
            10,
        )
        .unwrap();
        let result = tree.semantic_query(&query);
        assert_eq!(result.matches.len(), 3);
        assert_eq!(result.matches[0].tier, MatchTier::NameSubstring);
        assert_eq!(result.matches[1].field, Some(MatchField::Description));
        assert_eq!(result.matches[2].field, Some(MatchField::Value));
    }

    #[test]
    fn semantic_query_ranks_exact_name_before_other_substrings() {
        let exact = node(AxRole::Button, Some("save"));
        let name = node(AxRole::Button, Some("Save account"));
        let mut description = node(AxRole::Button, Some("Other"));
        description.description = Some("save account".into());
        let mut value = node(AxRole::TextField, Some("Status"));
        value.value = Some("save complete".into());
        let tree = tree(vec![value, description, name, exact]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("save".into()), None, Vec::new()).unwrap(),
            None,
            10,
        )
        .unwrap();
        let tiers: Vec<_> = tree
            .semantic_query(&query)
            .matches
            .into_iter()
            .map(|m| m.tier)
            .collect();
        assert_eq!(
            tiers,
            vec![
                MatchTier::ExactName,
                MatchTier::NameSubstring,
                MatchTier::DescriptionSubstring,
                MatchTier::ValueSubstring,
            ]
        );
    }

    #[test]
    fn secure_values_never_match_or_leave_query_output() {
        let mut secret = node(AxRole::TextField, Some("Password"));
        secret.value = Some("needle-secret".into());
        secret.states.secure = true;
        secret.states.editable = true;
        let tree = tree(vec![secret]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("needle".into()), None, Vec::new()).unwrap(),
            None,
            10,
        )
        .unwrap();
        assert!(tree.semantic_query(&query).matches.is_empty());
    }

    #[test]
    fn role_and_state_filters_are_anded_and_conflicts_are_rejected() {
        let selector = SemanticSelector::new(
            None,
            Some(AxRole::Button),
            vec![
                SemanticState::Visible,
                SemanticState::Enabled,
                SemanticState::Enabled,
            ],
        )
        .unwrap();
        assert_eq!(selector.states().len(), 2);
        let err = SemanticSelector::new(
            None,
            Some(AxRole::Button),
            vec![SemanticState::Enabled, SemanticState::Disabled],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SemanticQueryError::ContradictoryStates { .. }
        ));
    }

    #[test]
    fn semantic_scope_is_unique_and_restricts_target_search() {
        let mut first = node(AxRole::Document, Some("First"));
        first.children.push(node(AxRole::Button, Some("Save")));
        let mut second = node(AxRole::Document, Some("Second"));
        second.children.push(node(AxRole::Button, Some("Save")));
        let tree = tree(vec![first, second]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("save".into()), Some(AxRole::Button), Vec::new()).unwrap(),
            Some(
                SemanticSelector::new(Some("second".into()), Some(AxRole::Document), Vec::new())
                    .unwrap(),
            ),
            10,
        )
        .unwrap();
        let result = tree.semantic_query(&query);
        assert!(matches!(result.scope, ScopeResolution::Resolved(_)));
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn semantic_scope_reports_not_found_and_ambiguous() {
        let first = node(AxRole::Document, Some("Page"));
        let second = node(AxRole::Document, Some("Page"));
        let tree = tree(vec![first, second]);
        let target = SemanticSelector::new(Some("save".into()), None, Vec::new()).unwrap();
        let missing = SemanticQuery::new(
            target.clone(),
            Some(
                SemanticSelector::new(Some("missing".into()), Some(AxRole::Document), Vec::new())
                    .unwrap(),
            ),
            10,
        )
        .unwrap();
        assert_eq!(
            tree.semantic_query(&missing).scope,
            ScopeResolution::NotFound
        );
        let ambiguous = SemanticQuery::new(
            target,
            Some(
                SemanticSelector::new(Some("page".into()), Some(AxRole::Document), Vec::new())
                    .unwrap(),
            ),
            10,
        )
        .unwrap();
        assert_eq!(
            tree.semantic_query(&ambiguous).scope,
            ScopeResolution::Ambiguous { observed: 2 }
        );
    }

    #[test]
    fn context_and_result_count_are_fixed_and_bounded() {
        let mut group = node(AxRole::Group, Some("Account form"));
        group.children = vec![
            node(AxRole::Label, Some("before")),
            node(AxRole::Button, Some("Save one")),
            node(AxRole::Button, Some("Save two")),
        ];
        let tree = tree(vec![group]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("save".into()), Some(AxRole::Button), Vec::new()).unwrap(),
            None,
            1,
        )
        .unwrap();
        let result = tree.semantic_query(&query);
        assert_eq!(result.matches_in_walk, 2);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.omitted_by_max_results, 1);
        assert!(result.matches[0].context.contains("Account form"));
        assert!(result.matches[0].context.contains("Save two"));
    }

    #[test]
    fn semantic_query_context_has_fixed_ancestor_neighbor_and_child_limits() {
        let mut target = node(AxRole::Button, Some("Save target"));
        target.children = (1..=4)
            .map(|index| node(AxRole::Label, Some(&format!("child {index}"))))
            .collect();
        let mut innermost = node(AxRole::Group, Some("ancestor 6"));
        innermost.children = vec![
            node(AxRole::Label, Some("previous sibling")),
            target,
            node(AxRole::Label, Some("next sibling")),
        ];
        let nested = (1..=5).rev().fold(innermost, |child, index| {
            let mut parent = node(AxRole::Group, Some(&format!("ancestor {index}")));
            parent.children.push(child);
            parent
        });
        let tree = tree(vec![nested]);
        let query = SemanticQuery::new(
            SemanticSelector::new(Some("save target".into()), Some(AxRole::Button), Vec::new())
                .unwrap(),
            None,
            10,
        )
        .unwrap();

        let context = &tree.semantic_query(&query).matches[0].context;
        assert!(context.contains("… higher ancestors omitted"));
        assert!(!context.contains("ancestor 1"));
        assert!(!context.contains("ancestor 2"));
        for name in ["ancestor 3", "ancestor 4", "ancestor 5", "ancestor 6"] {
            assert!(context.contains(name));
        }
        assert!(context.contains("previous sibling"));
        assert!(context.contains("next sibling"));
        for name in ["child 1", "child 2", "child 3"] {
            assert!(context.contains(name));
        }
        assert!(!context.contains("child 4"));
    }
}
