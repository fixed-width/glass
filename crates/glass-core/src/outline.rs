//! Agent-facing outline rendering for an [`AxTree`] — the same information
//! [`AxTree::to_outline`] renders, with pure structural scaffolding collapsed away.
//! Pure text work, no OS dependencies.
//!
//! Toolkits (GTK, Jetpack Compose, UIKit) wrap real widgets in towers of unnamed
//! single-child containers. Those lines carry no name, value, state or addressable meaning,
//! yet they dominate the outline an agent pays tokens to read.
//!
//! Collapsing them is lossless for addressing: ids are assigned over the FULL tree before
//! rendering, and `click_element` resolves against the full cached tree — so an elided node
//! keeps its id and stays clickable. Only the text shrinks.
//!
//! [`AxTree::to_outline`] deliberately does NOT compact: `scroll_to_element` compares
//! consecutive outlines to detect saturation, and elided wrappers still carry bounds that
//! change as content scrolls. Compacting there could make a still-scrolling view render
//! identically twice and be declared saturated.

use std::fmt::Write as _;

use crate::accessibility::{AxNode, AxRole, AxTree};

/// Whether `node` is pure structural scaffolding — a container that conveys nothing its
/// single child does not already convey.
///
/// Every conjunct is load-bearing:
/// - a semantic role is never scaffolding, even unnamed;
/// - a named or valued container is real structure an agent may reason about;
/// - a **focusable** container is actable: Jetpack Compose surfaces a real button as a
///   clickable `Group` with the role lost (see `accessibility::element_match`), so eliding
///   it would hide a button that is still clickable — invisible but addressable;
/// - a multi-child container conveys grouping; a single-child one does not.
fn is_scaffolding(node: &AxNode) -> bool {
    matches!(node.role, AxRole::Group | AxRole::Other)
        && node.name.is_none()
        && node.value.is_none()
        && !node.states.focusable
        && node.children.len() == 1
}

/// Write one node's line: `#<id> <Role> "<name>" (<x>,<y> <w>x<h>) [<states>]`, name/bounds/
/// states elided when absent, two spaces of indent per depth level.
///
/// Shared by [`AxTree::to_outline`] and [`render_compact`] so the two renders cannot drift —
/// a test asserts they are identical for a tree with nothing to elide.
pub(crate) fn write_line(node: &AxNode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let _ = write!(out, "{indent}#{} {:?}", node.id.0, node.role);
    if let Some(name) = &node.name {
        let _ = write!(out, " {name:?}");
    }
    if let Some(b) = &node.bounds {
        let _ = write!(out, " ({},{} {}x{})", b.x, b.y, b.width, b.height);
    }
    let states = node.states.active();
    if !states.is_empty() {
        let _ = write!(out, " [{}]", states.join(","));
    }
    out.push('\n');
}

/// Render the agent-facing outline: scaffolding chains collapsed, truncation disclosed.
/// Total — this cannot fail.
pub fn render_compact(tree: &AxTree) -> String {
    let mut out = String::new();
    // The root anchors the outline and is rendered unconditionally, even when it is itself
    // an unnamed single-child container.
    write_line(&tree.root, 0, &mut out);
    write_children(&tree.root, 1, &mut out);
    if let Some(t) = tree.truncated {
        let _ = writeln!(out, "{}", t.notice());
    }
    out
}

/// Render `parent`'s children at `depth`. A scaffolding child is not rendered; the first
/// non-scaffolding descendant takes its place at the SAME depth, so a chain of wrappers
/// collapses entirely rather than leaving a growing indent.
fn write_children(parent: &AxNode, depth: usize, out: &mut String) {
    for child in &parent.children {
        let mut node = child;
        while is_scaffolding(node) {
            // `is_scaffolding` guarantees exactly one child; `first()` keeps this total
            // rather than relying on that invariant holding at an index.
            let Some(only) = node.children.first() else {
                break;
            };
            node = only;
        }
        write_line(node, depth, out);
        write_children(node, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessibility::{AxNode, AxNodeId, AxRect, AxStates, Truncation, TruncationLimit};

    /// A node with the given role/name and no children.
    fn node(role: AxRole, name: Option<&str>) -> AxNode {
        AxNode {
            id: AxNodeId(0),
            role,
            raw_role: String::new(),
            name: name.map(Into::into),
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        }
    }

    /// Wrap `child` in `depth` unnamed, non-focusable, single-child Groups.
    fn wrap(child: AxNode, depth: usize) -> AxNode {
        (0..depth).fold(child, |acc, _| {
            let mut g = node(AxRole::Group, None);
            g.children = vec![acc];
            g
        })
    }

    /// A tree rooted at a Window whose single child is `child`, ids assigned.
    fn tree_of(child: AxNode) -> AxTree {
        let mut root = node(AxRole::Window, Some("App"));
        root.children = vec![child];
        let mut t = AxTree::new(root);
        t.assign_ids();
        t
    }

    #[test]
    fn a_single_wrapper_group_is_elided() {
        let t = tree_of(wrap(node(AxRole::Button, Some("Save")), 1));
        assert!(!render_compact(&t).contains("Group"));
    }

    #[test]
    fn a_deep_wrapper_chain_collapses_entirely() {
        let t = tree_of(wrap(node(AxRole::Button, Some("Save")), 10));
        assert_eq!(
            render_compact(&t).lines().count(),
            2,
            "Window + Button only:\n{}",
            render_compact(&t)
        );
    }

    #[test]
    fn the_button_keeps_its_id_after_collapsing() {
        // 10 wrappers => Window #0, wrappers #1..#10, Button #11. Compaction must not renumber.
        let t = tree_of(wrap(node(AxRole::Button, Some("Save")), 10));
        assert!(
            render_compact(&t).contains("#11 Button"),
            "ids are assigned over the FULL tree:\n{}",
            render_compact(&t)
        );
    }

    #[test]
    fn a_named_group_is_kept() {
        let mut g = node(AxRole::Group, Some("Toolbar"));
        g.children = vec![node(AxRole::Button, Some("Save"))];
        assert!(render_compact(&tree_of(g)).contains("Toolbar"));
    }

    #[test]
    fn a_focusable_group_is_kept() {
        // Jetpack Compose surfaces a real button as a clickable Group with the role lost.
        // Eliding it would hide a button that is still clickable — invisible but addressable.
        let mut g = node(AxRole::Group, None);
        g.states = AxStates {
            focusable: true,
            ..AxStates::default()
        };
        g.children = vec![node(AxRole::Label, Some("Submit"))];
        assert!(
            render_compact(&tree_of(g)).contains("Group"),
            "a focusable Group is actable and must never be elided"
        );
    }

    #[test]
    fn a_group_with_a_value_is_kept() {
        let mut g = node(AxRole::Group, None);
        g.value = Some("42".into());
        g.children = vec![node(AxRole::Label, Some("Count"))];
        assert!(render_compact(&tree_of(g)).contains("Group"));
    }

    #[test]
    fn a_multi_child_group_is_kept() {
        let mut g = node(AxRole::Group, None);
        g.children = vec![
            node(AxRole::Button, Some("Ok")),
            node(AxRole::Button, Some("Cancel")),
        ];
        assert!(
            render_compact(&tree_of(g)).contains("Group"),
            "a multi-child container conveys real grouping"
        );
    }

    #[test]
    fn a_childless_group_is_kept() {
        assert!(render_compact(&tree_of(node(AxRole::Group, None))).contains("Group"));
    }

    #[test]
    fn a_semantic_role_is_never_elided() {
        let mut list = node(AxRole::List, None);
        list.children = vec![node(AxRole::ListItem, Some("Row"))];
        assert!(render_compact(&tree_of(list)).contains("List"));
    }

    #[test]
    fn the_root_is_never_elided() {
        // An unnamed single-child Group ROOT still anchors the outline.
        let mut root = node(AxRole::Group, None);
        root.children = vec![node(AxRole::Button, Some("Save"))];
        let mut t = AxTree::new(root);
        t.assign_ids();
        assert!(render_compact(&t).starts_with("#0 Group"));
    }

    #[test]
    fn a_tree_with_nothing_to_elide_renders_identically_to_the_full_outline() {
        let t = tree_of(node(AxRole::Button, Some("Save")));
        assert_eq!(render_compact(&t), t.to_outline());
    }

    #[test]
    fn the_truncation_notice_is_rendered() {
        let mut t = tree_of(node(AxRole::Button, Some("Save")));
        t.truncated = Some(Truncation {
            limit: TruncationLimit::Nodes,
            nodes_walked: 1500,
        });
        assert!(render_compact(&t).contains("truncated"));
    }

    #[test]
    fn bounds_and_states_survive_compaction() {
        let mut b = node(AxRole::Button, Some("Save"));
        b.bounds = Some(AxRect {
            x: 12,
            y: 40,
            width: 80,
            height: 24,
        });
        b.states = AxStates {
            enabled: true,
            ..AxStates::default()
        };
        assert!(render_compact(&tree_of(wrap(b, 3))).contains("(12,40 80x24) [enabled]"));
    }
}
