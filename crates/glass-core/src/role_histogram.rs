//! Tally an accessibility tree by native token — the evidence behind a role mapping.
//!
//! Every node keeps the backend's own token in [`AxNode::raw_role`]; anything the backend
//! does not map shows up as [`AxRole::Other`]. Counting `(token, role)` pairs over a real
//! app's tree answers two questions at once: which tokens an app actually emits, and which
//! of them glass still ignores.

use crate::accessibility::{AxNode, AxRole, AxTree};

/// One `(native token, mapped role)` bucket and how often it occurred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleTally {
    /// The backend's native token, as carried in [`AxNode::raw_role`].
    pub raw_role: String,
    /// The role that token produced.
    pub role: AxRole,
    /// Number of nodes in the tree with this exact pair.
    pub count: usize,
}

/// Tally every node by `(raw_role, role)`.
///
/// Unmapped buckets ([`AxRole::Other`]) come first — they are what a probe run is looking for
/// — then buckets by descending count, then by token, then by role (using Debug format as a
/// tiebreaker so the same token mapping to multiple roles orders consistently). The order is
/// total, so two runs of the same app diff cleanly.
pub fn role_histogram(tree: &AxTree) -> Vec<RoleTally> {
    let mut tallies: Vec<RoleTally> = Vec::new();
    visit(&tree.root, &mut tallies);
    tallies.sort_by(|a, b| {
        // `false` sorts before `true`, so ordering on "is mapped" puts the unmapped buckets —
        // what a probe run is looking for — first.
        let mapped = |t: &RoleTally| t.role != AxRole::Other;
        mapped(a)
            .cmp(&mapped(b))
            .then(b.count.cmp(&a.count))
            .then(a.raw_role.cmp(&b.raw_role))
            .then(format!("{:?}", a.role).cmp(&format!("{:?}", b.role)))
    });
    tallies
}

fn visit(node: &AxNode, tallies: &mut Vec<RoleTally>) {
    match tallies
        .iter_mut()
        .find(|t| t.role == node.role && t.raw_role == node.raw_role)
    {
        Some(t) => t.count += 1,
        None => tallies.push(RoleTally {
            raw_role: node.raw_role.clone(),
            role: node.role,
            count: 1,
        }),
    }
    for child in &node.children {
        visit(child, tallies);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessibility::{AxNode, AxNodeId, AxStates};

    fn node(raw: &str, role: AxRole, children: Vec<AxNode>) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role,
            raw_role: raw.to_string(),
            name: None,
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children,
        }
    }

    #[test]
    fn tallies_tokens_with_unmapped_first_then_by_count() {
        let tree = AxTree::new(node(
            "window",
            AxRole::Window,
            vec![
                node("push button", AxRole::Button, vec![]),
                node("push button", AxRole::Button, vec![]),
                node("ruler", AxRole::Other, vec![]),
                node("push button", AxRole::Button, vec![]),
            ],
        ));
        let h = role_histogram(&tree);
        assert_eq!(
            h,
            vec![
                RoleTally {
                    raw_role: "ruler".into(),
                    role: AxRole::Other,
                    count: 1
                },
                RoleTally {
                    raw_role: "push button".into(),
                    role: AxRole::Button,
                    count: 3
                },
                RoleTally {
                    raw_role: "window".into(),
                    role: AxRole::Window,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn same_token_mapping_two_ways_gets_two_buckets() {
        // A backend may key on more than the token alone (a control type plus a pattern, a
        // role plus a subrole), so one token can legitimately produce two roles. When the same
        // token maps to multiple roles, they are ordered by role Debug format as a tiebreaker.
        let tree = AxTree::new(node(
            "root",
            AxRole::Window,
            vec![
                node("row", AxRole::ListItem, vec![]),
                node("row", AxRole::TreeItem, vec![]),
            ],
        ));
        let h = role_histogram(&tree);
        assert_eq!(
            h,
            vec![
                RoleTally {
                    raw_role: "root".into(),
                    role: AxRole::Window,
                    count: 1
                },
                RoleTally {
                    raw_role: "row".into(),
                    role: AxRole::ListItem,
                    count: 1
                },
                RoleTally {
                    raw_role: "row".into(),
                    role: AxRole::TreeItem,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn token_tiebreak_sorts_lexicographically_when_unmapped_and_count_tie() {
        // When two buckets tie on unmapped status and count, they sort by token lexicographically.
        let tree = AxTree::new(node(
            "unused",
            AxRole::Window,
            vec![
                node("beta", AxRole::Other, vec![]),
                node("alpha", AxRole::Other, vec![]),
            ],
        ));
        let h = role_histogram(&tree);
        assert_eq!(
            h,
            vec![
                RoleTally {
                    raw_role: "alpha".into(),
                    role: AxRole::Other,
                    count: 1
                },
                RoleTally {
                    raw_role: "beta".into(),
                    role: AxRole::Other,
                    count: 1
                },
                RoleTally {
                    raw_role: "unused".into(),
                    role: AxRole::Window,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn empty_token_is_kept_as_its_own_bucket() {
        let tree = AxTree::new(node("", AxRole::Other, vec![]));
        let h = role_histogram(&tree);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].raw_role, "");
    }
}
