//! Tally an accessibility tree by whether its nodes carry a description — the evidence
//! behind whether a platform's secondary label is worth wiring up.
//!
//! [`AxNode::description`] exists on every backend, but nothing sources it until that
//! backend's own PR lands; until then every tree reports zero. Counting how many of a real
//! app's nodes actually carry one turns "does this app's UI have descriptions" into a number
//! a probe run prints, rather than a guess from documentation.

use crate::accessibility::{AxNode, AxTree};

/// One described node a probe run wants to see, not just count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescribedSample {
    /// The backend's native role string, as carried in [`AxNode::raw_role`].
    pub raw_role: String,
    pub name: Option<String>,
    pub description: String,
}

/// How many of a tree's nodes carry a description, and a sample of which ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptionCensus {
    /// Total nodes walked.
    pub nodes: usize,
    /// Nodes whose `description` is `Some`.
    pub described: usize,
    /// One [`DescribedSample`] per described node.
    pub samples: Vec<DescribedSample>,
}

/// Walk every node once, counting the tree and sampling every description found.
///
/// Samples sort by `description`, then `raw_role`, then `name` — a total order, so two runs
/// of the same app diff cleanly, mirroring why [`crate::role_histogram`] sorts totally.
/// `described == 0` is itself the finding a probe run is after: it means this platform's
/// tree carries no descriptions yet, not that the walk found nothing.
pub fn description_census(tree: &AxTree) -> DescriptionCensus {
    let mut census = DescriptionCensus {
        nodes: 0,
        described: 0,
        samples: Vec::new(),
    };
    visit(&tree.root, &mut census);
    census.samples.sort_by(|a, b| {
        a.description
            .cmp(&b.description)
            .then(a.raw_role.cmp(&b.raw_role))
            .then(a.name.cmp(&b.name))
    });
    census
}

fn visit(node: &AxNode, census: &mut DescriptionCensus) {
    census.nodes += 1;
    if let Some(description) = &node.description {
        census.described += 1;
        census.samples.push(DescribedSample {
            raw_role: node.raw_role.clone(),
            name: node.name.clone(),
            description: description.clone(),
        });
    }
    for child in &node.children {
        visit(child, census);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessibility::{AxNodeId, AxRole, AxStates};

    fn described(raw_role: &str, name: Option<&str>, description: Option<&str>) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role: AxRole::Other,
            raw_role: raw_role.to_string(),
            name: name.map(str::to_string),
            description: description.map(str::to_string),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: Vec::new(),
        }
    }

    fn tree_of(children: Vec<AxNode>) -> AxTree {
        AxTree::new(AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "root".to_string(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children,
        })
    }

    #[test]
    fn census_counts_described_nodes_and_samples_them() {
        let tree = tree_of(vec![
            described("push button", Some("Save"), Some("Saves and closes")),
            described("push button", Some("Open"), None),
            described("toggle button", None, Some("Bold")),
        ]);
        let census = description_census(&tree);
        // The root the helper wraps these in counts too, and carries no description.
        assert_eq!(census.nodes, 4);
        assert_eq!(census.described, 2);
        assert_eq!(
            census
                .samples
                .iter()
                .map(|s| s.description.as_str())
                .collect::<Vec<_>>(),
            vec!["Bold", "Saves and closes"]
        );
    }

    #[test]
    fn census_of_a_tree_with_no_descriptions_is_empty_not_absent() {
        let census =
            description_census(&tree_of(vec![described("push button", Some("Save"), None)]));
        assert_eq!(census.described, 0);
        assert!(census.samples.is_empty());
        // A run that read a real tree and found nothing is a finding; it must be
        // distinguishable from a run that read nothing at all.
        assert_eq!(census.nodes, 2);
    }
}
