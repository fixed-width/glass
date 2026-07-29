//! Tally an accessibility tree by whether its nodes carry a description — the evidence
//! behind whether a platform's secondary label is worth wiring up.
//!
//! Counting how many of a real app's nodes carry one turns "does this app's UI have
//! descriptions" into a number a probe run prints, rather than a guess from documentation.
//!
//! [`AxNode::description`] exists on every backend, but a reader that leaves it `None` makes
//! the count zero by construction, for every app — a fact about glass, not about the platform.
//! Only a caller knows which of the two it has, so [`description_census_report`] takes a
//! [`DescriptionSourcing`] and prints it beside the number.

use std::fmt::Write as _;

use crate::accessibility::{AxNode, AxTree};

/// One described node a probe run wants to see, not just count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescribedSample {
    /// The backend's native role string, as carried in [`AxNode::raw_role`].
    pub raw_role: String,
    /// The node's name, alongside the description.
    pub name: Option<String>,
    /// The node's description — the reason this sample exists.
    pub description: String,
}

/// How many of a tree's nodes carry a description, and a sample of which ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptionCensus {
    /// Total nodes walked.
    pub nodes: usize,
    /// One [`DescribedSample`] per described node.
    pub samples: Vec<DescribedSample>,
}

impl DescriptionCensus {
    /// Nodes whose `description` is `Some`. Derived from [`Self::samples`], which holds one
    /// entry per described node, so the two cannot disagree.
    pub fn described(&self) -> usize {
        self.samples.len()
    }
}

/// Whether the reader that produced a tree sources [`AxNode::description`] at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionSourcing {
    /// The reader reads its platform's secondary label, so the count describes the app.
    Sourced,
    /// The reader leaves `description: None`, so the count is 0 for every app.
    Unsourced,
}

/// Walk every node once, counting the tree and sampling every description found.
///
/// Samples sort by `description`, then `raw_role`, then `name` — a total order, so two runs
/// of the same app diff cleanly, mirroring why [`crate::role_histogram`] sorts totally.
pub fn description_census(tree: &AxTree) -> DescriptionCensus {
    let mut census = DescriptionCensus {
        nodes: 0,
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

/// The census as a printable block: one summary line, then one line per sample. Returns a
/// `String` rather than printing so a caller that saves a report can fold it in.
///
/// `sourcing` says on the summary line which kind of zero this is: a reader that leaves the
/// field `None` counts zero on every app, which unqualified reads as a finding about the
/// platform.
pub fn description_census_report(
    label: &str,
    tree: &AxTree,
    sourcing: DescriptionSourcing,
) -> String {
    let census = description_census(tree);
    let caveat = match sourcing {
        DescriptionSourcing::Sourced => "",
        DescriptionSourcing::Unsourced => {
            " (this backend's reader leaves description: None, so the count is 0 whatever the app exposes)"
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{label}: {} of {} nodes described{caveat}",
        census.described(),
        census.nodes
    );
    for sample in &census.samples {
        let _ = writeln!(
            out,
            "  {} name={:?} desc={:?}",
            sample.raw_role, sample.name, sample.description
        );
    }
    out
}

fn visit(node: &AxNode, census: &mut DescriptionCensus) {
    census.nodes += 1;
    if let Some(description) = &node.description {
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
        // The described toggle sits UNDER an undescribed wrapper, not beside it: in a real
        // app the described widgets are levels down, so a walk that never recursed would
        // report zero there while passing a flat fixture.
        let mut wrapper = described("filler", None, None);
        wrapper.children = vec![described("toggle button", None, Some("Bold"))];
        let tree = tree_of(vec![
            described("push button", Some("Save"), Some("Saves and closes")),
            described("push button", Some("Open"), None),
            wrapper,
        ]);
        let census = description_census(&tree);
        // The root the helper wraps these in counts too, and carries no description.
        assert_eq!(census.nodes, 5);
        assert_eq!(census.described(), 2);
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
    fn census_orders_ties_by_raw_role_then_name() {
        // Same description on every node (plausible: several "OK" controls on one screen), so
        // this pins both tie-break clauses: `raw_role` separates "checkbox" from "push button",
        // then `name` separates the two "push button" samples from each other.
        let tree = tree_of(vec![
            described("push button", Some("Zeta"), Some("OK")),
            described("checkbox", Some("Enable"), Some("OK")),
            described("push button", Some("Alpha"), Some("OK")),
        ]);
        let census = description_census(&tree);
        let order: Vec<(&str, Option<&str>)> = census
            .samples
            .iter()
            .map(|s| (s.raw_role.as_str(), s.name.as_deref()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("checkbox", Some("Enable")),
                ("push button", Some("Alpha")),
                ("push button", Some("Zeta")),
            ]
        );
    }

    #[test]
    fn census_of_a_tree_with_no_descriptions_is_empty_not_absent() {
        let census =
            description_census(&tree_of(vec![described("push button", Some("Save"), None)]));
        assert_eq!(census.described(), 0);
        assert!(census.samples.is_empty());
        // A run that read a real tree and found nothing is a finding; it must be
        // distinguishable from a run that read nothing at all.
        assert_eq!(census.nodes, 2);
    }

    #[test]
    fn a_report_over_an_unsourced_reader_says_the_zero_is_not_an_observation() {
        let tree = tree_of(vec![described("push button", Some("Save"), None)]);
        let report = description_census_report("app", &tree, DescriptionSourcing::Unsourced);
        assert!(
            report.starts_with("app: 0 of 2 nodes described ("),
            "the qualifier must sit on the number's own line: {report}"
        );
        assert!(report.contains("description: None"), "{report}");
    }

    #[test]
    fn a_report_over_a_sourcing_reader_is_the_bare_count_and_its_samples() {
        let tree = tree_of(vec![described("push button", Some("Save"), Some("Saves"))]);
        let report = description_census_report("app", &tree, DescriptionSourcing::Sourced);
        assert_eq!(
            report,
            "app: 1 of 2 nodes described\n  push button name=Some(\"Save\") desc=\"Saves\"\n"
        );
    }
}
