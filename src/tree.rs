use std::sync::Arc;

use crate::{ArrayVec, Item, Node, SumTree, Summary, TreeError, TreeResult};

impl<T: Item, const CAP: usize> SumTree<T, CAP> {
    /// An empty tree: the root is an empty leaf, so every tree has a root.
    pub fn new() -> Self {
        Self::from_node(Node::Leaf {
            summary: T::Summary::empty(),
            items: ArrayVec::new(),
            item_summaries: ArrayVec::new(),
        })
    }

    fn from_node(node: Node<T, CAP>) -> Self {
        SumTree {
            root: Arc::new(node),
        }
    }

    pub fn summary(&self) -> &T::Summary {
        self.root.summary()
    }

    pub fn append(&self, other: SumTree<T, CAP>) {
        /* append doesn't go item by item. It attaches
         * other's nodes as whole subtrees:
         *   1. Same height: if the right edge of self and the
         *   root of other are at the same height, other's
         *   children go into the last node at that level,
         *   or the two root nodes simply become siblings.
         *   2. Different heights: walk down the right edge
         *   of the taller tree until you reach a node at
         *   the shorter tree's height, then add the shorter
         *   tree there as a child. That may overflow the node,
         *   and the overflow moves up exactly like a split
         *   in push. If the root splits, you get a new root,
         *   same as in push.
         *   3. Leaf level: if two leaves meet and both are
         *   nearly empty, merge their items instead of leaving
         *   two underfull leaves side by side.
         *
         *   Cost is O(log n) for the whole tree, compared with O(m log n)
         *   for pushing m items one by one.
         * No items get copied either: other's nodes are shared through Arc::clone.
         * */
        todo!("Append a whole tree");
    }

    pub fn slice(&self) {}

    /// Every item, in order.
    pub fn items(&self) -> Vec<T> {
        let mut out = Vec::new();
        self.root.collect_items(&mut out);
        out
    }

    /// Checks the tree's shape rules, returning the first one broken:
    /// - every leaf is at the same depth;
    /// - every node but the root holds between `CAP / 2` and `CAP` children or
    ///   items, and an internal root holds at least 2;
    /// - every cached summary equals the sum of what's under it.
    pub fn check(&self) -> Result<(), String>
    where
        T::Summary: PartialEq + std::fmt::Debug,
    {
        self.root.check(true, 0, &mut None)
    }

    pub fn push(&mut self, item: T) {
        // If the root split, the tree grows a level: a new root holding [old root, right].
        if let Some(right) = Arc::make_mut(&mut self.root).push(item) {
            let left = SumTree {
                root: Arc::clone(&self.root),
            };
            let child_summaries: ArrayVec<T::Summary, CAP> =
                [left.summary().clone(), right.summary().clone()]
                    .into_iter()
                    .collect();
            let summary = sum_all(&child_summaries);

            self.root = Arc::new(Node::Internal {
                summary,
                children: [left, right].into_iter().collect(),
                child_summaries,
            });
        }
    }
}

/// Moves the back half of `v` into a new vec and returns it; `v` keeps the
/// front half. The returned half gets the extra element when the length is odd.
fn split<T, const CAP: usize>(v: &mut ArrayVec<T, CAP>) -> TreeResult<ArrayVec<T, CAP>> {
    if v.len() <= 1 {
        return Err(TreeError::NoEffectError);
    }
    let mid = v.len() / 2;
    Ok(v.drain(mid..).collect())
}

fn sum_all<S: Summary, const CAP: usize>(children: &ArrayVec<S, CAP>) -> S {
    let mut s: S = S::empty();
    for child in children.iter() {
        s.concat(child);
    }
    return s;
}

impl<T: Item, const CAP: usize> Node<T, CAP> {
    pub fn summary(&self) -> &T::Summary {
        match self {
            Node::Internal { summary, .. } | Node::Leaf { summary, .. } => summary,
        }
    }

    fn collect_items(&self, out: &mut Vec<T>) {
        match self {
            Node::Internal { children, .. } => {
                for child in children {
                    child.root.collect_items(out);
                }
            }
            Node::Leaf { items, .. } => out.extend(items.iter().cloned()),
        }
    }

    fn check(
        &self,
        is_root: bool,
        depth: usize,
        leaf_depth: &mut Option<usize>,
    ) -> Result<(), String>
    where
        T::Summary: PartialEq + std::fmt::Debug,
    {
        let min = if is_root { 0 } else { CAP / 2 };
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
            } => {
                let min = if is_root { 2 } else { min };
                if children.len() < min {
                    return Err(format!(
                        "internal node at depth {depth} has {} children, want at least {min}",
                        children.len()
                    ));
                }
                if children.len() != child_summaries.len() {
                    return Err(format!(
                        "internal node at depth {depth} has {} children but {} child summaries",
                        children.len(),
                        child_summaries.len()
                    ));
                }
                for (i, (child, s)) in children.iter().zip(child_summaries).enumerate() {
                    if child.summary() != s {
                        return Err(format!(
                            "child {i} at depth {depth} has summary {:?}, its parent cached {s:?}",
                            child.summary()
                        ));
                    }
                    child.root.check(false, depth + 1, leaf_depth)?;
                }
                let want = sum_all(child_summaries);
                if *summary != want {
                    return Err(format!(
                        "internal node at depth {depth} cached {summary:?}, its children sum to {want:?}"
                    ));
                }
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                if *leaf_depth.get_or_insert(depth) != depth {
                    return Err(format!(
                        "leaves at depths {} and {depth}",
                        leaf_depth.unwrap()
                    ));
                }
                if items.len() < min {
                    return Err(format!(
                        "leaf at depth {depth} has {} items, want at least {min}",
                        items.len()
                    ));
                }
                if items.len() != item_summaries.len() {
                    return Err(format!(
                        "leaf at depth {depth} has {} items but {} item summaries",
                        items.len(),
                        item_summaries.len()
                    ));
                }
                for (i, (item, s)) in items.iter().zip(item_summaries).enumerate() {
                    if item.summary() != *s {
                        return Err(format!(
                            "item {i} at depth {depth} has summary {:?}, its leaf cached {s:?}",
                            item.summary()
                        ));
                    }
                }
                let want = sum_all(item_summaries);
                if *summary != want {
                    return Err(format!(
                        "leaf at depth {depth} cached {summary:?}, its items sum to {want:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn push(&mut self, in_item: T) -> Option<SumTree<T, CAP>> {
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
            } => {
                let child = children
                    .last_mut()
                    .expect("An empty internal node? It's unthinkable. It's unheard of!");
                let right = Arc::make_mut(&mut child.root).push(in_item);
                *child_summaries.last_mut().unwrap() = child.summary().clone();

                let Some(right) = right else {
                    *summary = sum_all(child_summaries);
                    return None;
                };
                let right_summary = right.summary().clone();

                if children.is_full() {
                    // NOTE: It's not you, it's me
                    let mut rightems = split(children).expect("full children should have >1 item");
                    let mut summarightems =
                        split(child_summaries).expect("full child summaries should have >1 item");
                    rightems.push(right);
                    summarightems.push(right_summary);

                    *summary = sum_all(child_summaries);
                    Some(SumTree::from_node(Node::Internal {
                        summary: sum_all(&summarightems),
                        children: rightems,
                        child_summaries: summarightems,
                    }))
                } else {
                    children.push(right);
                    child_summaries.push(right_summary);
                    *summary = sum_all(child_summaries);
                    None
                }
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                if items.is_full() {
                    // Keep the left half in place; move the right half to a new sibling.
                    let mut right_items = split(items).expect("full leaf has >1 item");
                    let mut right_sums = split(item_summaries).expect("full leaf has >1 item");
                    right_sums.push(in_item.summary());
                    right_items.push(in_item);

                    *summary = T::Summary::empty();
                    for s in item_summaries.iter() {
                        summary.concat(s);
                    }
                    let mut right_summary = T::Summary::empty();
                    for s in right_sums.iter() {
                        right_summary.concat(s);
                    }

                    Some(SumTree::from_node(Node::Leaf {
                        summary: right_summary,
                        items: right_items,
                        item_summaries: right_sums,
                    }))
                } else {
                    let s = in_item.summary();
                    summary.concat(&s);
                    item_summaries.push(s);
                    items.push(in_item);
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn split_preserves_order_and_balance(v in prop::collection::vec(any::<u8>(), 2..=12)) {
            let mut l: ArrayVec<u8, 12> = v.iter().copied().collect();
            let r = split(&mut l).unwrap();
            prop_assert_eq!([&l[..], &r[..]].concat(), v);
            prop_assert!(l.len().abs_diff(r.len()) <= 1);
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct CountSum {
        count: usize,
        sum: u64,
    }

    impl Summary for CountSum {
        fn empty() -> Self {
            CountSum { count: 0, sum: 0 }
        }
        fn concat(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
        }
    }

    impl Item for u64 {
        type Summary = CountSum;
        fn summary(&self) -> CountSum {
            CountSum {
                count: 1,
                sum: *self,
            }
        }
    }

    /// Checks every stored summary matches its contents and all leaves sit at the same depth.
    fn check<const CAP: usize>(
        tree: &SumTree<u64, CAP>,
        depth: usize,
        leaf_depth: &mut Option<usize>,
    ) -> Vec<u64> {
        match &*tree.root {
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                assert_eq!(*leaf_depth.get_or_insert(depth), depth, "unbalanced");
                assert_eq!(summary, &sum_all(item_summaries));
                items.to_vec()
            }
            Node::Internal {
                summary,
                children,
                child_summaries,
            } => {
                assert!(!children.is_empty());
                assert_eq!(summary, &sum_all(child_summaries));
                let mut out = Vec::new();
                for (child, s) in children.iter().zip(child_summaries) {
                    assert_eq!(child.summary(), s);
                    out.extend(check(child, depth + 1, leaf_depth));
                }
                out
            }
        }
    }

    proptest! {
        #[test]
        fn push_keeps_order_summaries_and_balance(n in 0u64..2000) {
            let mut tree: SumTree<u64> = SumTree::new();
            for i in 0..n {
                tree.push(i);
            }
            let items = check(&tree, 0, &mut None);
            prop_assert_eq!(items, (0..n).collect::<Vec<_>>());
            prop_assert_eq!(tree.summary(), &CountSum { count: n as usize, sum: (0..n).sum() });
        }
    }

    #[test]
    fn split_too_small_is_no_effect() {
        let mut empty: ArrayVec<u8, 12> = ArrayVec::new();
        assert_eq!(split(&mut empty), Err(TreeError::NoEffectError));
        let mut one: ArrayVec<u8, 12> = [7].into_iter().collect();
        assert_eq!(split(&mut one), Err(TreeError::NoEffectError));
        assert_eq!(one.as_slice(), &[7]); // untouched on error
    }
}
