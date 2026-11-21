use std::{iter, sync::Arc};

use crate::{ArrayVec, Item, Node, SumTree, Summary};

impl<T: Item, const CAP: usize> Default for SumTree<T, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

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

    /// A tree that's one leaf holding `items`, whose summaries are already
    /// known. Any number up to `CAP` is fine: a lone leaf is a root.
    pub(crate) fn from_leaf(items: &[T], summaries: &[T::Summary]) -> Self {
        debug_assert_eq!(items.len(), summaries.len());
        let item_summaries: ArrayVec<T::Summary, CAP> = summaries.iter().cloned().collect();
        Self::from_node(Node::Leaf {
            summary: sum_all(&item_summaries),
            items: items.iter().cloned().collect(),
            item_summaries,
        })
    }

    pub fn summary(&self) -> &T::Summary {
        self.root.summary()
    }

    /// Levels above the leaves: 0 for a tree that's a single leaf.
    pub fn height(&self) -> u8 {
        self.root.height()
    }

    /// True if the tree holds no items. Only the root can be empty.
    pub fn is_empty(&self) -> bool {
        matches!(&*self.root, Node::Leaf { items, .. } if items.is_empty())
    }

    pub fn append(&mut self, other: SumTree<T, CAP>) {
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

        if other.is_empty() {
            // Nothing to add.
        } else if self.is_empty() {
            // Nothing to keep: the result is just `other`.
            self.root = other.root;
        } else if self.height() >= other.height() {
            // Find other's level on our right edge.
            self.push_tree(other, Side::Back);
        } else {
            // `other` is taller, so mirror it: find our level on other's left
            // edge. Swap first, so `self` ends up holding the result.
            let front = std::mem::replace(self, other);
            self.push_tree(front, Side::Front);
        }
    }

    /// Merges `other` onto this tree's `side` edge, at the level matching its
    /// height (which must not be more than ours).
    fn push_tree(&mut self, other: SumTree<T, CAP>, side: Side) {
        if let Some(split_off) = Arc::make_mut(&mut self.root).push_tree(other, side) {
            self.grow_root(split_off, side);
        }
    }

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
        if let Some(right) = Arc::make_mut(&mut self.root).push(item) {
            self.grow_root(right, Side::Back);
        }
    }

    /// The first item, if there is one. O(log n).
    pub fn first(&self) -> Option<&T> {
        self.root.edge_item(Side::Front)
    }

    /// The last item, if there is one. O(log n).
    pub fn last(&self) -> Option<&T> {
        self.root.edge_item(Side::Back)
    }

    /// Changes the first item in place, and every summary above it. Does
    /// nothing to an empty tree. Copies only the path down to it, if shared.
    pub fn update_first(&mut self, f: impl FnOnce(&mut T)) {
        Arc::make_mut(&mut self.root).update_edge_item(Side::Front, f);
    }

    /// Changes the last item in place, and every summary above it. Does
    /// nothing to an empty tree. Copies only the path down to it, if shared.
    pub fn update_last(&mut self, f: impl FnOnce(&mut T)) {
        Arc::make_mut(&mut self.root).update_edge_item(Side::Back, f);
    }

    /// The root split off a sibling on its `side`: the tree grows a level,
    /// with a new root holding both, in order.
    fn grow_root(&mut self, split_off: SumTree<T, CAP>, side: Side) {
        let old = SumTree {
            root: Arc::clone(&self.root),
        };
        let height = old.height() + 1;
        let [left, right] = match side {
            Side::Back => [old, split_off],
            Side::Front => [split_off, old],
        };
        let child_summaries: ArrayVec<T::Summary, CAP> =
            [left.summary().clone(), right.summary().clone()]
                .into_iter()
                .collect();

        self.root = Arc::new(Node::Internal {
            height,
            summary: sum_all(&child_summaries),
            children: [left, right].into_iter().collect(),
            child_summaries,
        });
    }
}

/// Which end of a node or tree to work at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Front,
    Back,
}

/// Adds `incoming` to `v` at its `side`, keeping order. If they don't all
/// fit, splits them evenly instead: `v` keeps the half away from `side`, and
/// the half at `side` (the larger one when odd) is returned. Never overfills
/// `v`, so the total can be anything up to `2 * CAP`.
fn extend_or_split<E, const CAP: usize>(
    v: &mut ArrayVec<E, CAP>,
    incoming: impl ExactSizeIterator<Item = E>,
    side: Side,
) -> Option<ArrayVec<E, CAP>> {
    let total = v.len() + incoming.len();
    debug_assert!(total <= 2 * CAP, "{total} entries can't fill two nodes");
    let keep = if total <= CAP { total } else { total / 2 };

    // Lay everything out in order, then deal `keep` to `v` and the rest to
    // the split-off half, from whichever end `side` says.
    let old: ArrayVec<E, CAP> = v.drain(..).collect();
    let split_off = match side {
        Side::Back => {
            let mut all = old.into_iter().chain(incoming);
            v.extend(all.by_ref().take(keep));
            all.collect::<ArrayVec<E, CAP>>()
        }
        Side::Front => {
            let mut all = incoming.chain(old);
            let split_off = all
                .by_ref()
                .take(total - keep)
                .collect::<ArrayVec<E, CAP>>();
            v.extend(all);
            split_off
        }
    };
    (total > CAP).then_some(split_off)
}

fn sum_all<S: Summary, const CAP: usize>(children: &ArrayVec<S, CAP>) -> S {
    let mut s: S = S::empty();
    for child in children.iter() {
        s.concat(child);
    }
    s
}

impl<T: Item, const CAP: usize> Node<T, CAP> {
    pub(crate) fn summary(&self) -> &T::Summary {
        match self {
            Node::Internal { summary, .. } | Node::Leaf { summary, .. } => summary,
        }
    }

    pub(crate) fn height(&self) -> u8 {
        match self {
            Node::Internal { height, .. } => *height,
            Node::Leaf { .. } => 0,
        }
    }

    /// The item at this node's `side` edge, down through children.
    fn edge_item(&self, side: Side) -> Option<&T> {
        match self {
            Node::Internal { children, .. } => {
                let child = match side {
                    Side::Front => children.first(),
                    Side::Back => children.last(),
                };
                child?.root.edge_item(side)
            }
            Node::Leaf { items, .. } => match side {
                Side::Front => items.first(),
                Side::Back => items.last(),
            },
        }
    }

    /// Runs `f` on the item at this node's `side` edge, then refreshes the
    /// cached summaries on the way back up.
    fn update_edge_item(&mut self, side: Side, f: impl FnOnce(&mut T)) {
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
                ..
            } => {
                let i = match side {
                    Side::Front => 0,
                    Side::Back => children.len() - 1,
                };
                let child = &mut children[i];
                Arc::make_mut(&mut child.root).update_edge_item(side, f);
                child_summaries[i] = child.summary().clone();
                *summary = sum_all(child_summaries);
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let i = match side {
                    Side::Front => 0,
                    Side::Back if items.is_empty() => return,
                    Side::Back => items.len() - 1,
                };
                let Some(item) = items.get_mut(i) else {
                    return;
                };
                f(item);
                item_summaries[i] = item.summary();
                *summary = sum_all(item_summaries);
            }
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
                height,
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
                    if child.height() + 1 != *height {
                        return Err(format!(
                            "child {i} at depth {depth} has height {}, its parent has height {height}",
                            child.height()
                        ));
                    }
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

    /// Merges `other` onto this node's `side` edge, at the level matching its
    /// height. Returns a new sibling, for our `side`, if this node overflowed.
    fn push_tree(&mut self, other: SumTree<T, CAP>, side: Side) -> Option<SumTree<T, CAP>> {
        debug_assert!(other.height() <= self.height());

        if self.height() > other.height() {
            // We're taller: `other` belongs further down the `side` edge.
            return self.update_edge_child(side, |child| child.push_tree(other, side));
        }

        // Same height: `other` is our sibling, so its entries join ours. Moves
        // them if nothing else shares `other`'s root, clones them if something does.
        match Arc::unwrap_or_clone(other.root) {
            Node::Leaf {
                items,
                item_summaries,
                ..
            } => self.extend_leaf(items.into_iter(), item_summaries.into_iter(), side),
            Node::Internal {
                children,
                child_summaries,
                ..
            } => self.extend_internal(children.into_iter(), child_summaries.into_iter(), side),
        }
    }

    pub(crate) fn push(&mut self, in_item: T) -> Option<SumTree<T, CAP>> {
        match self {
            Node::Internal { .. } => {
                self.update_edge_child(Side::Back, |child| child.push(in_item))
            }
            Node::Leaf { .. } => {
                let s = in_item.summary();
                self.extend_leaf(iter::once(in_item), iter::once(s), Side::Back)
            }
        }
    }

    /// Runs `f` on the child at our `side` edge, then takes in whatever it
    /// split off as a new child beside it. Returns a new sibling, for our
    /// `side`, if that overflowed us.
    fn update_edge_child(
        &mut self,
        side: Side,
        f: impl FnOnce(&mut Node<T, CAP>) -> Option<SumTree<T, CAP>>,
    ) -> Option<SumTree<T, CAP>> {
        let Node::Internal {
            summary,
            children,
            child_summaries,
            ..
        } = self
        else {
            unreachable!("leaves have no children");
        };
        let i = match side {
            Side::Front => 0,
            Side::Back => children.len() - 1,
        };
        let child = children
            .get_mut(i)
            .expect("An empty internal node? It's unthinkable. It's unheard of!");
        let split_off = f(Arc::make_mut(&mut child.root));
        child_summaries[i] = child.summary().clone();

        match split_off {
            None => {
                *summary = sum_all(child_summaries);
                None
            }
            Some(split_off) => {
                let s = split_off.summary().clone();
                self.extend_internal(iter::once(split_off), iter::once(s), side)
            }
        }
    }

    /// Adds items, with their summaries, at this leaf's `side`. Returns the
    /// new sibling, for that side, if they didn't all fit.
    fn extend_leaf(
        &mut self,
        new_items: impl ExactSizeIterator<Item = T>,
        new_summaries: impl ExactSizeIterator<Item = T::Summary>,
        side: Side,
    ) -> Option<SumTree<T, CAP>> {
        let Node::Leaf {
            summary,
            items,
            item_summaries,
        } = self
        else {
            unreachable!("extend_leaf on an internal node");
        };
        // Same lengths in, so both split at the same place.
        let split_items = extend_or_split(items, new_items, side);
        let split_summaries = extend_or_split(item_summaries, new_summaries, side);
        *summary = sum_all(item_summaries);

        match (split_items, split_summaries) {
            (None, None) => None,
            (Some(items), Some(item_summaries)) => Some(SumTree::from_node(Node::Leaf {
                summary: sum_all(&item_summaries),
                items,
                item_summaries,
            })),
            _ => unreachable!("items and their summaries split differently"),
        }
    }

    /// Adds children, with their summaries, at this internal node's `side`.
    /// Returns the new sibling, for that side, if they didn't all fit.
    fn extend_internal(
        &mut self,
        new_children: impl ExactSizeIterator<Item = SumTree<T, CAP>>,
        new_summaries: impl ExactSizeIterator<Item = T::Summary>,
        side: Side,
    ) -> Option<SumTree<T, CAP>> {
        let Node::Internal {
            height,
            summary,
            children,
            child_summaries,
        } = self
        else {
            unreachable!("extend_internal on a leaf");
        };
        // NOTE: It's not you, it's me
        let split_children = extend_or_split(children, new_children, side);
        let split_summaries = extend_or_split(child_summaries, new_summaries, side);
        *summary = sum_all(child_summaries);

        match (split_children, split_summaries) {
            (None, None) => None,
            (Some(children), Some(child_summaries)) => Some(SumTree::from_node(Node::Internal {
                height: *height,
                summary: sum_all(&child_summaries),
                children,
                child_summaries,
            })),
            _ => unreachable!("children and their summaries split differently"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn extend_or_split_preserves_order_and_balance(
            v in prop::collection::vec(any::<u8>(), 0..=12),
            more in prop::collection::vec(any::<u8>(), 0..=12),
            front in any::<bool>(),
        ) {
            let side = if front { Side::Front } else { Side::Back };
            let mut kept: ArrayVec<u8, 12> = v.iter().copied().collect();
            let split_off = extend_or_split(&mut kept, more.iter().copied(), side);
            let all = match side {
                Side::Back => [&v[..], &more[..]].concat(),
                Side::Front => [&more[..], &v[..]].concat(),
            };
            match split_off {
                None => {
                    prop_assert!(all.len() <= 12);
                    prop_assert_eq!(kept.as_slice(), &all[..]);
                }
                Some(split_off) => {
                    prop_assert!(all.len() > 12);
                    let joined = match side {
                        Side::Back => [&kept[..], &split_off[..]].concat(),
                        Side::Front => [&split_off[..], &kept[..]].concat(),
                    };
                    prop_assert_eq!(joined, all);
                    prop_assert!(kept.len() <= split_off.len() && split_off.len() - kept.len() <= 1);
                }
            }
        }
    }

    /// Order-sensitive on purpose: `first` and `last` come out wrong if
    /// summaries are ever combined in the wrong order, which a plain count or
    /// sum can't show.
    #[derive(Clone, Debug, PartialEq)]
    pub struct CountSum {
        count: usize,
        sum: u64,
        first: Option<u64>,
        last: Option<u64>,
    }

    impl Summary for CountSum {
        fn empty() -> Self {
            CountSum {
                count: 0,
                sum: 0,
                first: None,
                last: None,
            }
        }
        fn concat(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
            self.first = self.first.or(other.first);
            self.last = other.last.or(self.last);
        }
    }

    impl Item for u64 {
        type Summary = CountSum;
        fn summary(&self) -> CountSum {
            CountSum {
                count: 1,
                sum: *self,
                first: Some(*self),
                last: Some(*self),
            }
        }
    }

    fn tree_of<const CAP: usize>(items: std::ops::Range<u64>) -> SumTree<u64, CAP> {
        let mut tree = SumTree::new();
        for i in items {
            tree.push(i);
        }
        tree
    }

    fn summary_of(items: std::ops::Range<u64>) -> CountSum {
        let mut s = CountSum::empty();
        for i in items {
            s.concat(&i.summary());
        }
        s
    }

    proptest! {
        #[test]
        fn push_keeps_order_summaries_and_balance(n in 0u64..2000) {
            let tree: SumTree<u64> = tree_of(0..n);
            tree.check().map_err(TestCaseError::fail)?;
            prop_assert_eq!(tree.items(), (0..n).collect::<Vec<_>>());
            prop_assert_eq!(tree.summary(), &summary_of(0..n));
        }

        /// Tiny nodes, so a few hundred items make trees several levels tall,
        /// and random sizes put the taller tree on either side.
        #[test]
        fn append_keeps_order_summaries_and_balance(a in 0u64..600, b in 0u64..600) {
            let left: SumTree<u64, 4> = tree_of(0..a);
            let right: SumTree<u64, 4> = tree_of(a..a + b);
            let mut tree = left.clone();
            tree.append(right.clone());

            tree.check().map_err(TestCaseError::fail)?;
            prop_assert_eq!(tree.items(), (0..a + b).collect::<Vec<_>>());
            prop_assert_eq!(tree.summary(), &summary_of(0..a + b));
            // The inputs are untouched.
            prop_assert_eq!(left.items(), (0..a).collect::<Vec<_>>());
            prop_assert_eq!(right.items(), (a..a + b).collect::<Vec<_>>());
        }

        /// Change both edge items in place: items, summaries and shape stay
        /// right all the way up, and a copy taken before is untouched.
        #[test]
        fn update_edges_in_place(n in 0u64..600, add in 1u64..100) {
            let tree: SumTree<u64, 4> = tree_of(0..n);
            let mut changed = tree.clone();
            changed.update_first(|x| *x += add);
            changed.update_last(|x| *x += add);

            let mut want: Vec<u64> = (0..n).collect();
            if let Some(x) = want.first_mut() {
                *x += add;
            }
            if let Some(x) = want.last_mut() {
                *x += add;
            }
            changed.check().map_err(TestCaseError::fail)?;
            prop_assert_eq!(changed.items(), want.clone());
            prop_assert_eq!(changed.first(), want.first());
            prop_assert_eq!(changed.last(), want.last());
            let mut s = CountSum::empty();
            for x in &want {
                s.concat(&x.summary());
            }
            prop_assert_eq!(changed.summary(), &s);
            prop_assert_eq!(tree.items(), (0..n).collect::<Vec<_>>());
        }
    }
}
