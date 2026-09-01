use super::*;
use crate::{ScopeResolution, SemanticQuery, SemanticQueryResult};

#[derive(Clone, Debug)]
pub struct FindElementsParams {
    pub query: SemanticQuery,
    pub max_nodes: Option<usize>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct FindElementsOutcome {
    pub result: SemanticQueryResult,
    pub matched: bool,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub timed_out_by: Option<crate::Whose>,
}

impl Glass {
    pub fn find_elements(&mut self, params: &FindElementsParams) -> Result<FindElementsOutcome> {
        self.find_elements_by(params, Deadline::UNBOUNDED)
    }

    pub fn find_elements_by(
        &mut self,
        params: &FindElementsParams,
        sequence_deadline: Deadline,
    ) -> Result<FindElementsOutcome> {
        self.set_a11y_limits(params.max_nodes)?;
        let poll = self.poll_accessibility_until(
            200,
            params.timeout_ms,
            sequence_deadline,
            "find elements",
            |tree| tree.semantic_query(&params.query),
            |result| {
                !result.matches.is_empty()
                    || matches!(result.scope, ScopeResolution::Ambiguous { .. })
            },
        )?;
        let matched = !poll.observation.matches.is_empty();
        Ok(FindElementsOutcome {
            result: poll.observation,
            matched,
            timed_out: params.timeout_ms > 0 && !matched && poll.timed_out_by.is_some(),
            elapsed_ms: poll.elapsed_ms,
            timed_out_by: poll.timed_out_by,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::*;
    use crate::{
        AxNodeId, AxRole, SemanticSelector, SemanticState, Truncation, TruncationLimit, WalkLimits,
    };
    use std::sync::atomic::Ordering;

    fn query(text: &str) -> SemanticQuery {
        SemanticQuery::new(
            SemanticSelector::new(Some(text.into()), None, Vec::new()).unwrap(),
            None,
            10,
        )
        .unwrap()
    }

    #[test]
    fn find_elements_reads_fresh_and_returns_actionable_ids() {
        let mut tree = fake_tree();
        tree.root.children[0].name = Some("Save account".into());
        let mut glass = glass_with_a11y(FakePlatform::new(100, 100), tree);
        glass.start(&spec()).unwrap();
        let out = glass
            .find_elements(&FindElementsParams {
                query: query("save"),
                max_nodes: None,
                timeout_ms: 0,
            })
            .unwrap();
        assert_eq!(out.result.matches.len(), 1);
        glass
            .click_element(out.result.matches[0].element.id)
            .unwrap();
    }

    #[test]
    fn find_elements_waits_for_delayed_publication_and_caches_the_final_tree() {
        let first = fake_tree();
        let mut second = fake_tree();
        second.root.children[0].name = Some("Save account".into());
        let mut glass = glass_with_a11y_seq(FakePlatform::new(100, 100), vec![first, second]);
        glass.start(&spec()).unwrap();
        let out = glass
            .find_elements(&FindElementsParams {
                query: query("save account"),
                max_nodes: None,
                timeout_ms: 500,
            })
            .unwrap();
        assert!(out.matched);
        assert!(!out.timed_out);
        assert_eq!(out.result.matches.len(), 1);
    }

    #[test]
    fn scope_ambiguity_returns_after_one_fresh_read() {
        let mut tree = fake_tree();
        tree.root.children.push(tree.root.children[0].clone());
        tree.assign_ids();
        let scope = SemanticSelector::new(None, Some(AxRole::Button), Vec::new()).unwrap();
        let query = SemanticQuery::new(
            SemanticSelector::new(None, None, vec![SemanticState::Enabled]).unwrap(),
            Some(scope),
            10,
        )
        .unwrap();
        let (mut glass, walks) =
            glass_with_a11y_counted(FakePlatform::new(100, 100), vec![tree], None);
        glass.start(&spec()).unwrap();
        let out = glass
            .find_elements(&FindElementsParams {
                query,
                max_nodes: None,
                timeout_ms: 500,
            })
            .unwrap();
        assert!(matches!(
            out.result.scope,
            ScopeResolution::Ambiguous { observed: 2 }
        ));
        assert!(!out.timed_out);
        assert_eq!(walks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn positive_timeout_without_a_match_is_soft() {
        let mut glass = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        glass.start(&spec()).unwrap();
        let out = glass
            .find_elements(&FindElementsParams {
                query: query("absent"),
                max_nodes: None,
                timeout_ms: 25,
            })
            .unwrap();
        assert!(!out.matched);
        assert!(out.timed_out);
    }

    #[test]
    fn zero_timeout_is_one_read_not_a_timeout() {
        let mut glass = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        glass.start(&spec()).unwrap();
        let out = glass
            .find_elements(&FindElementsParams {
                query: query("absent"),
                max_nodes: None,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!out.matched);
        assert!(!out.timed_out);
    }

    #[test]
    fn find_elements_uses_default_walk_limits_when_max_nodes_is_omitted() {
        let (mut glass, ctx_log) = glass_with_a11y_ctx(FakePlatform::new(100, 100), fake_tree());
        glass.start(&spec()).unwrap();

        glass
            .find_elements(&FindElementsParams {
                query: query("save"),
                max_nodes: None,
                timeout_ms: 0,
            })
            .unwrap();

        assert_eq!(
            ctx_log.lock().unwrap().as_ref().unwrap().limits,
            WalkLimits::DEFAULT
        );
    }

    #[test]
    fn find_elements_max_nodes_zero_lifts_only_the_node_cap() {
        let (mut glass, ctx_log) = glass_with_a11y_ctx(FakePlatform::new(100, 100), fake_tree());
        glass.start(&spec()).unwrap();

        glass
            .find_elements(&FindElementsParams {
                query: query("save"),
                max_nodes: Some(0),
                timeout_ms: 0,
            })
            .unwrap();

        assert_eq!(
            ctx_log.lock().unwrap().as_ref().unwrap().limits,
            WalkLimits::from_max_nodes(Some(0))
        );
    }

    #[test]
    fn find_elements_preserves_truncation_and_incomplete_search_data() {
        let mut tree = fake_tree();
        tree.truncated = Some(Truncation {
            limit: TruncationLimit::Nodes,
            limit_value: 2,
            nodes_walked: 2,
        });
        let mut glass = glass_with_a11y(FakePlatform::new(100, 100), tree);
        glass.start(&spec()).unwrap();

        let out = glass
            .find_elements(&FindElementsParams {
                query: query("absent"),
                max_nodes: None,
                timeout_ms: 0,
            })
            .unwrap();

        assert!(out.result.tree_truncated.is_some());
        assert!(!out.result.search_complete);
    }

    #[test]
    fn find_elements_soft_timeout_caches_the_last_fresh_tree() {
        let first = fake_tree();
        let mut second = fake_tree();
        let mut preceding = second.root.children[0].clone();
        preceding.role = AxRole::Label;
        preceding.name = Some("Before".into());
        second.root.children.insert(0, preceding);
        second.assign_ids();
        assert!(first.find(AxNodeId(2)).is_none());
        assert!(second.find(AxNodeId(2)).is_some());
        let mut glass = glass_with_a11y_seq(FakePlatform::new(100, 100), vec![first, second]);
        glass.start(&spec()).unwrap();

        let out = glass
            .find_elements(&FindElementsParams {
                query: query("absent"),
                max_nodes: None,
                timeout_ms: 25,
            })
            .unwrap();

        assert!(out.timed_out);
        glass.click_element(AxNodeId(2)).unwrap();
    }
}
