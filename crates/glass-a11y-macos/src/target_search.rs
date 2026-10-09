#![forbid(unsafe_code)]

use glass_core::{Result, WalkBudget, WalkLimits};

pub(crate) trait TargetTree {
    type Element;

    fn require(&mut self) -> Result<()>;
    fn matches(&mut self, element: &Self::Element) -> Result<bool>;
    fn children(&mut self, element: &Self::Element) -> Result<Vec<Self::Element>>;
    fn should_skip(&mut self, element: &Self::Element) -> Result<bool>;
}

/// Prove a unique match within a complete bounded traversal before returning an element.
pub(crate) fn find_unique<T: TargetTree>(
    tree: &mut T,
    root: T::Element,
    limits: WalkLimits,
) -> Result<Option<T::Element>> {
    let mut budget = WalkBudget::with_limits(limits);
    let mut found = None;
    let complete = visit(tree, root, 0, &mut budget, &mut found)?;
    tree.require()?;
    Ok(if complete && budget.truncation().is_none() {
        found
    } else {
        None
    })
}

fn visit<T: TargetTree>(
    tree: &mut T,
    element: T::Element,
    depth: usize,
    budget: &mut WalkBudget,
    found: &mut Option<T::Element>,
) -> Result<bool> {
    tree.require()?;
    budget.visit();
    let matched = tree.matches(&element)?;
    if matched && found.is_some() {
        return Ok(false);
    }
    let children = tree.children(&element)?;
    if matched {
        *found = Some(element);
    }
    if !children.is_empty() {
        if !budget.may_explore_children(depth) {
            return Ok(false);
        }
        for (scanned, child) in children.into_iter().enumerate() {
            tree.require()?;
            if !budget.may_visit_sibling(scanned) {
                return Ok(false);
            }
            if !tree.should_skip(&child)? && !visit(tree, child, depth + 1, budget, found)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::GlassError;

    struct Tree {
        children: Vec<Vec<usize>>,
        matches: Vec<usize>,
        unreadable: Option<usize>,
        checks_left: usize,
    }

    impl Tree {
        fn new(children: Vec<Vec<usize>>, matches: Vec<usize>) -> Self {
            Self {
                children,
                matches,
                unreadable: None,
                checks_left: usize::MAX,
            }
        }
    }

    impl TargetTree for Tree {
        type Element = usize;

        fn require(&mut self) -> Result<()> {
            if self.checks_left == 0 {
                return Err(GlassError::deadline_not_started("target search"));
            }
            self.checks_left -= 1;
            Ok(())
        }

        fn matches(&mut self, element: &usize) -> Result<bool> {
            Ok(self.matches.contains(element))
        }

        fn children(&mut self, element: &usize) -> Result<Vec<usize>> {
            if self.unreadable == Some(*element) {
                return Err(GlassError::Backend("unreadable subtree".into()));
            }
            Ok(self.children[*element].clone())
        }

        fn should_skip(&mut self, _element: &usize) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn finds_target_after_sibling_insertion_changes_its_position() {
        let mut tree = Tree::new(vec![vec![2, 1], vec![], vec![]], vec![1]);
        assert_eq!(
            find_unique(&mut tree, 0, WalkLimits::DEFAULT).unwrap(),
            Some(1)
        );
    }

    #[test]
    fn refuses_two_matching_controls() {
        let mut tree = Tree::new(vec![vec![1, 2], vec![], vec![]], vec![1, 2]);
        assert_eq!(
            find_unique(&mut tree, 0, WalkLimits::DEFAULT).unwrap(),
            None
        );
    }

    #[test]
    fn unreadable_subtree_cannot_establish_uniqueness() {
        let mut tree = Tree::new(vec![vec![1, 2], vec![], vec![]], vec![1]);
        tree.unreadable = Some(2);
        assert!(matches!(
            find_unique(&mut tree, 0, WalkLimits::DEFAULT),
            Err(GlassError::Backend(_))
        ));
    }

    #[test]
    fn node_limit_cannot_establish_uniqueness() {
        let mut tree = Tree::new(vec![vec![1, 2], vec![], vec![]], vec![1]);
        let limits = WalkLimits {
            nodes: 2,
            ..WalkLimits::DEFAULT
        };
        assert_eq!(find_unique(&mut tree, 0, limits).unwrap(), None);
    }

    #[test]
    fn sibling_limit_cannot_establish_uniqueness() {
        let mut tree = Tree::new(vec![vec![1, 2], vec![], vec![]], vec![1]);
        let limits = WalkLimits {
            siblings: 1,
            ..WalkLimits::DEFAULT
        };
        assert_eq!(find_unique(&mut tree, 0, limits).unwrap(), None);
    }

    #[test]
    fn depth_limit_cannot_establish_uniqueness() {
        let mut tree = Tree::new(vec![vec![1], vec![]], vec![0]);
        let limits = WalkLimits {
            depth: 0,
            ..WalkLimits::DEFAULT
        };
        assert_eq!(find_unique(&mut tree, 0, limits).unwrap(), None);
    }

    #[test]
    fn complete_tree_exactly_at_node_limit_proves_a_match() {
        let mut tree = Tree::new(vec![vec![1], vec![]], vec![1]);
        let limits = WalkLimits {
            nodes: 2,
            ..WalkLimits::DEFAULT
        };
        assert_eq!(find_unique(&mut tree, 0, limits).unwrap(), Some(1));
    }

    #[test]
    fn expired_deadline_after_a_match_cannot_return_it() {
        let mut tree = Tree::new(vec![vec![1], vec![]], vec![0]);
        tree.checks_left = 2;
        assert!(
            find_unique(&mut tree, 0, WalkLimits::DEFAULT)
                .unwrap_err()
                .bound_owner()
                .is_some()
        );
    }
}
