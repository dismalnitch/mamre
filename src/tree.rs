use std::{iter, sync::Arc};

use crate::cursor::{Bias, passes};
use crate::{ArrayVec, Dimension, Item, Node, SeekTarget, SumTree, Summary};

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

    /// A tree whose root holds `children`, whose summaries are already
    /// known: a slice of some node's children, shared, not copied. Any
    /// number up to `CAP`: one is just that child, none an empty tree.
    pub(crate) fn from_children(children: &[SumTree<T, CAP>], summaries: &[T::Summary]) -> Self {
        debug_assert_eq!(children.len(), summaries.len());
        let (first, only) = match children {
            [] => return SumTree::new(),
            [only] => (only, true),
            [first, ..] => (first, false),
        };
        // An internal node has two children or more: one is just itself.
        if only {
            return first.clone();
        }
        let child_summaries: ArrayVec<T::Summary, CAP> = summaries.iter().cloned().collect();
        Self::from_node(Node::Internal {
            height: first.height() + 1,
            summary: sum_all(&child_summaries),
            children: children.iter().cloned().collect(),
            child_summaries,
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

    /// Whether `other` is the very same tree, not just an equal one: true
    /// for a clone until either is changed. O(1), so it's the cheap way to
    /// ask "has this changed since I last looked?" within one program.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }

    /// Changes the item a seek for `target` lands on (see
    /// [`Cursor::seek_forward`](crate::cursor::Cursor::seek_forward) for
    /// `bias`), in place, and refreshes every summary above it. Returns what
    /// `f` returns, or `None` without calling it if the target is past the
    /// last item.
    ///
    /// This is how a tree of trees stays current: an item that holds a tree
    /// (a section holding its text, say) is changed through here, and its
    /// new summary rolls up. O(log n); a shared tree copies only the path
    /// down to the item.
    pub fn update_at<D: Dimension<T::Summary>, R>(
        &mut self,
        target: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let path = self.root.path_to::<D>(target, bias)?;
        Some(Arc::make_mut(&mut self.root).update_path(&path, f))
    }

    /// Changes the item a seek for `target` lands on, in place, and puts
    /// the items `f` returns right after it: up to `CAP` of them. Like
    /// [`update_at`](Self::update_at), only the path down to the item is
    /// copied if shared, and a node that overflows splits in two on the way
    /// back up. Returns what `f` returns, or `None` without calling it if
    /// the target is past the last item.
    ///
    /// To put items before it instead, have `f` swap the first of them in
    /// and hand the old item back after the rest. O(log n), where
    /// [`replace`](Self::replace) slices and rejoins the tree.
    pub fn splice_at<D, R, I>(
        &mut self,
        target: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
        f: impl FnOnce(&mut T) -> (R, I),
    ) -> Option<R>
    where
        D: Dimension<T::Summary>,
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let path = self.root.path_to::<D>(target, bias)?;
        let (out, split_off) = Arc::make_mut(&mut self.root).splice_path(&path, f);
        if let Some(split_off) = split_off {
            self.grow_root(split_off, Side::Back);
        }
        Some(out)
    }

    /// Removes the item a seek for `target` lands on, in place, and returns
    /// it: `None` if the target is past the last item. Only the path down to
    /// it is copied if shared; a node left too small on the way back up
    /// borrows from a sibling, or merges with it when both fit in one, and
    /// a root left with one child gives way to it. O(log n), where
    /// [`remove`](Self::remove) slices and rejoins the tree.
    pub fn remove_at<D: Dimension<T::Summary>>(
        &mut self,
        target: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
    ) -> Option<T> {
        let path = self.root.path_to::<D>(target, bias)?;
        let item = Arc::make_mut(&mut self.root).remove_path(&path);
        while let Node::Internal { children, .. } = &*self.root
            && children.len() == 1
        {
            let Node::Internal { children, .. } = Arc::make_mut(&mut self.root) else {
                unreachable!("matched above")
            };
            *self = children.pop().expect("one child");
        }
        Some(item)
    }

    /// Changes the first item in place, and every summary above it. `None`,
    /// without calling `f`, for an empty tree.
    pub fn update_first<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let path = self.root.edge_path(Side::Front)?;
        Some(Arc::make_mut(&mut self.root).update_path(&path, f))
    }

    /// Changes the last item in place, and every summary above it. `None`,
    /// without calling `f`, for an empty tree.
    pub fn update_last<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let path = self.root.edge_path(Side::Back)?;
        Some(Arc::make_mut(&mut self.root).update_path(&path, f))
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
    let n = incoming.len();
    let total = v.len() + n;
    debug_assert!(total <= 2 * CAP, "{total} entries can't fill two nodes");
    let mut incoming = incoming;

    // Only live entries move: no node-sized temporaries, which for big
    // items (a rope's chunks) cost more than everything else an edit does.
    if total <= CAP {
        v.extend(incoming);
        if side == Side::Front {
            v.rotate_right(n);
        }
        return None;
    }
    let keep = total / 2;
    let mut split_off = ArrayVec::new();
    match side {
        // In order, `v` then `incoming`: `v` keeps the first `keep`.
        Side::Back => {
            if keep <= v.len() {
                split_off.extend(v.drain(keep..));
                split_off.extend(incoming);
            } else {
                v.extend(incoming.by_ref().take(keep - v.len()));
                split_off.extend(incoming);
            }
        }
        // In order, `incoming` then `v`: `v` keeps the last `keep`.
        Side::Front => {
            let give = total - keep;
            if give <= n {
                split_off.extend(incoming.by_ref().take(give));
                let rest = n - give;
                v.extend(incoming);
                v.rotate_right(rest);
            } else {
                split_off.extend(incoming);
                split_off.extend(v.drain(..give - n));
            }
        }
    }
    Some(split_off)
}

/// Puts `new` (emptying it) into `v` right after entry `i`. If they don't
/// all fit, `v` keeps the first half and the rest is returned, for a new
/// sibling after it. Only live entries move.
fn insert_after<E, const CAP: usize>(
    v: &mut ArrayVec<E, CAP>,
    i: usize,
    new: &mut ArrayVec<E, CAP>,
) -> Option<ArrayVec<E, CAP>> {
    let total = v.len() + new.len();
    if total <= CAP {
        for (k, e) in new.drain(..).enumerate() {
            v.insert(i + 1 + k, e);
        }
        return None;
    }
    // In order: `v` up to `i`, then `new`, then the rest of `v`.
    let mut tail: ArrayVec<E, CAP> = v.drain(i + 1..).collect();
    let keep = total / 2;
    let mut split_off = ArrayVec::new();
    if v.len() > keep {
        split_off.extend(v.drain(keep..));
    }
    for e in new.drain(..).chain(tail.drain(..)) {
        if v.len() < keep {
            v.push(e);
        } else {
            split_off.push(e);
        }
    }
    Some(split_off)
}

/// Child `i` has too few entries: it merges with a sibling if both fit in
/// one node, and otherwise the two share their entries evenly. Siblings are
/// at the same height, so both are leaves or both internal.
fn mend<T: Item, const CAP: usize>(
    children: &mut ArrayVec<SumTree<T, CAP>, CAP>,
    child_summaries: &mut ArrayVec<T::Summary, CAP>,
    i: usize,
) {
    let l = if i > 0 { i - 1 } else { i };
    let (left, right) = children.split_at_mut(l + 1);
    let (left, right) = (&mut left[l], &mut right[0]);
    let merge = left.root.len() + right.root.len() <= CAP;
    let (left_node, right_node) = (
        Arc::make_mut(&mut left.root),
        Arc::make_mut(&mut right.root),
    );
    match (left_node, right_node) {
        (
            Node::Leaf {
                summary: ls,
                items: li,
                item_summaries: lis,
            },
            Node::Leaf {
                summary: rs,
                items: ri,
                item_summaries: ris,
            },
        ) => {
            share(li, ri, merge);
            share(lis, ris, merge);
            *ls = sum_all(lis);
            *rs = sum_all(ris);
        }
        (
            Node::Internal {
                summary: ls,
                children: lc,
                child_summaries: lcs,
                ..
            },
            Node::Internal {
                summary: rs,
                children: rc,
                child_summaries: rcs,
                ..
            },
        ) => {
            share(lc, rc, merge);
            share(lcs, rcs, merge);
            *ls = sum_all(lcs);
            *rs = sum_all(rcs);
        }
        _ => unreachable!("siblings are at the same height"),
    }
    if merge {
        children.remove(l + 1);
        child_summaries.remove(l + 1);
    } else {
        child_summaries[l + 1] = children[l + 1].summary().clone();
    }
    child_summaries[l] = children[l].summary().clone();
}

/// Moves entries between neighbours `left` and `right`: all of `right`
/// into `left` if `merge`, and otherwise so they hold half each (`left`
/// the smaller half, if odd). Order is kept.
fn share<E, const CAP: usize>(
    left: &mut ArrayVec<E, CAP>,
    right: &mut ArrayVec<E, CAP>,
    merge: bool,
) {
    if merge {
        left.extend(right.drain(..));
        return;
    }
    let total = left.len() + right.len();
    let keep = total / 2;
    if left.len() > keep {
        // Left's surplus goes to the front of right.
        let mut moved: ArrayVec<E, CAP> = left.drain(keep..).collect();
        let n = moved.len();
        right.extend(moved.drain(..));
        right.rotate_right(n);
    } else {
        let n = keep - left.len();
        left.extend(right.drain(..n));
    }
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

    /// The child (or item) index at each level, from here down to the item a
    /// seek for `target` lands on; `None` if it's past the last item. The
    /// same choice at each level as a cursor's seek: skip whole entries that
    /// end before the target, and go into the first one that doesn't.
    fn path_to<D: Dimension<T::Summary>>(
        &self,
        target: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
    ) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        let mut position = D::zero();
        let mut node = self;
        loop {
            let summaries: &[T::Summary] = match node {
                Node::Internal {
                    child_summaries, ..
                } => child_summaries,
                Node::Leaf { item_summaries, .. } => item_summaries,
            };
            // An entry the target doesn't pass holds it. Below the root
            // there always is one: the parent chose this node because its
            // end, which is its last entry's end, wasn't passed.
            let i = summaries.iter().position(|s| {
                let mut end = position.clone();
                end.add_summary(s);
                if passes(target.cmp_position(&end), bias) {
                    position = end;
                    false
                } else {
                    true
                }
            })?;
            path.push(i);
            match node {
                Node::Internal { children, .. } => node = &children[i].root,
                Node::Leaf { .. } => return Some(path),
            }
        }
    }

    /// The path to the item at this node's `side` edge; `None` if empty.
    fn edge_path(&self, side: Side) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        let mut node = self;
        loop {
            let len = match node {
                Node::Internal { children, .. } => children.len(),
                Node::Leaf { items, .. } => items.len(),
            };
            let i = match side {
                Side::Front if len > 0 => 0,
                Side::Back => len.checked_sub(1)?,
                Side::Front => return None,
            };
            path.push(i);
            match node {
                Node::Internal { children, .. } => node = &children[i].root,
                Node::Leaf { .. } => return Some(path),
            }
        }
    }

    /// Runs `f` on the item at the end of `path`, then refreshes the cached
    /// summaries on the way back up. Copies any node along the path that's
    /// shared.
    fn update_path<R>(&mut self, path: &[usize], f: impl FnOnce(&mut T) -> R) -> R {
        let i = path[0];
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
                ..
            } => {
                let child = &mut children[i];
                let out = Arc::make_mut(&mut child.root).update_path(&path[1..], f);
                child_summaries[i] = child.summary().clone();
                *summary = sum_all(child_summaries);
                out
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let out = f(&mut items[i]);
                item_summaries[i] = items[i].summary();
                *summary = sum_all(item_summaries);
                out
            }
        }
    }

    /// Runs `f` on the item at the end of `path` and puts what it returns
    /// right after it, refreshing summaries on the way back up. Returns a
    /// new sibling, to go after this node, if it overflowed.
    fn splice_path<R, I>(
        &mut self,
        path: &[usize],
        f: impl FnOnce(&mut T) -> (R, I),
    ) -> (R, Option<SumTree<T, CAP>>)
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let i = path[0];
        match self {
            Node::Internal {
                height,
                summary,
                children,
                child_summaries,
            } => {
                let child = &mut children[i];
                let (out, split_off) = Arc::make_mut(&mut child.root).splice_path(&path[1..], f);
                child_summaries[i] = child.summary().clone();
                let Some(split_off) = split_off else {
                    *summary = sum_all(child_summaries);
                    return (out, None);
                };
                let mut new_summaries: ArrayVec<T::Summary, CAP> =
                    iter::once(split_off.summary().clone()).collect();
                let mut new_children: ArrayVec<SumTree<T, CAP>, CAP> =
                    iter::once(split_off).collect();
                let more_summaries = insert_after(child_summaries, i, &mut new_summaries);
                let more_children = insert_after(children, i, &mut new_children);
                *summary = sum_all(child_summaries);
                let split_off =
                    more_children
                        .zip(more_summaries)
                        .map(|(children, child_summaries)| {
                            SumTree::from_node(Node::Internal {
                                height: *height,
                                summary: sum_all(&child_summaries),
                                children,
                                child_summaries,
                            })
                        });
                (out, split_off)
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let (out, extra) = f(&mut items[i]);
                item_summaries[i] = items[i].summary();
                let extra = extra.into_iter();
                assert!(extra.len() <= CAP, "splice_at puts in at most CAP items");
                let mut new_items: ArrayVec<T, CAP> = extra.collect();
                if new_items.is_empty() {
                    *summary = sum_all(item_summaries);
                    return (out, None);
                }
                let mut new_summaries: ArrayVec<T::Summary, CAP> =
                    new_items.iter().map(Item::summary).collect();
                let more_summaries = insert_after(item_summaries, i, &mut new_summaries);
                let more_items = insert_after(items, i, &mut new_items);
                *summary = sum_all(item_summaries);
                let split_off = more_items
                    .zip(more_summaries)
                    .map(|(items, item_summaries)| {
                        SumTree::from_node(Node::Leaf {
                            summary: sum_all(&item_summaries),
                            items,
                            item_summaries,
                        })
                    });
                (out, split_off)
            }
        }
    }

    /// Removes the item at the end of `path`, refreshing summaries and
    /// mending any child left too small on the way back up. This node may
    /// be left too small itself: its parent mends it.
    fn remove_path(&mut self, path: &[usize]) -> T {
        let i = path[0];
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
                ..
            } => {
                let child = &mut children[i];
                let item = Arc::make_mut(&mut child.root).remove_path(&path[1..]);
                child_summaries[i] = child.summary().clone();
                if child.root.len() < CAP / 2 && children.len() > 1 {
                    mend(children, child_summaries, i);
                }
                *summary = sum_all(child_summaries);
                item
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                item_summaries.remove(i);
                let item = items.remove(i);
                *summary = sum_all(item_summaries);
                item
            }
        }
    }

    /// How many entries: children, or items.
    fn len(&self) -> usize {
        match self {
            Node::Internal { children, .. } => children.len(),
            Node::Leaf { items, .. } => items.len(),
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
        // them if nothing else shares `other`'s root, clones them if something
        // does. Entry by entry either way, never the node whole: a node is as
        // big as a full leaf, which for big items is most of an edit's cost.
        let mut root = other.root;
        match Arc::get_mut(&mut root) {
            Some(Node::Leaf {
                items,
                item_summaries,
                ..
            }) => self.extend_leaf(items.drain(..), item_summaries.drain(..), side),
            Some(Node::Internal {
                children,
                child_summaries,
                ..
            }) => self.extend_internal(children.drain(..), child_summaries.drain(..), side),
            None => match &*root {
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => self.extend_leaf(items.iter().cloned(), item_summaries.iter().cloned(), side),
                Node::Internal {
                    children,
                    child_summaries,
                    ..
                } => self.extend_internal(
                    children.iter().cloned(),
                    child_summaries.iter().cloned(),
                    side,
                ),
            },
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

        /// Change the item a seek lands on, by count, either bias, including
        /// past the end: only that item changes, summaries roll up, shape
        /// holds, and a copy taken before is untouched.
        #[test]
        fn update_at_changes_the_item_a_seek_lands_on(
            n in 0u64..600,
            at in 0usize..610,
            left in any::<bool>(),
        ) {
            let bias = if left { Bias::Left } else { Bias::Right };
            let tree: SumTree<u64, 4> = tree_of(0..n);
            let mut changed = tree.clone();
            let got = changed.update_at::<Count, _>(&Count(at), bias, |x| {
                let was = *x;
                *x += 1000;
                was
            });

            // `Count(k)` with `Right` lands on item k; with `Left`, on the
            // item ending at k, which is k - 1 (or item 0 for k = 0).
            let lands = match bias {
                Bias::Right => at,
                Bias::Left => at.saturating_sub(1),
            };
            let mut want: Vec<u64> = (0..n).collect();
            match want.get_mut(lands) {
                Some(x) => {
                    prop_assert_eq!(got, Some(*x));
                    *x += 1000;
                }
                None => prop_assert_eq!(got, None),
            }
            changed.check().map_err(TestCaseError::fail)?;
            prop_assert_eq!(changed.items(), want.clone());
            let mut s = CountSum::empty();
            for x in &want {
                s.concat(&x.summary());
            }
            prop_assert_eq!(changed.summary(), &s);
            prop_assert_eq!(tree.items(), (0..n).collect::<Vec<_>>());
        }
    }

    /// Item count, as a dimension: ordered, so it's its own seek target.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Count(usize);

    impl Dimension<CountSum> for Count {
        fn zero() -> Self {
            Count(0)
        }
        fn add_summary(&mut self, s: &CountSum) {
            self.0 += s.count;
        }
    }

    /// The point of `update_at`: a tree of trees. Each section holds its own
    /// tree of words; changing one section's words rolls its new total up
    /// into the outline, and leaves the others shared.
    #[test]
    fn a_tree_of_trees_stays_current() {
        /// A section's words, as a tree of their own.
        #[derive(Clone)]
        struct Section(SumTree<u64, 4>);

        /// The outline counts sections, and rolls up their words.
        #[derive(Clone, Debug, PartialEq)]
        struct Outline {
            sections: usize,
            words: CountSum,
        }

        impl Summary for Outline {
            fn empty() -> Self {
                Outline {
                    sections: 0,
                    words: CountSum::empty(),
                }
            }
            fn concat(&mut self, other: &Self) {
                self.sections += other.sections;
                self.words.concat(&other.words);
            }
        }

        impl Item for Section {
            type Summary = Outline;
            fn summary(&self) -> Outline {
                // O(1): a tree's summary is cached at its root.
                Outline {
                    sections: 1,
                    words: self.0.summary().clone(),
                }
            }
        }

        /// Seek sections by how many come before.
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
        struct SectionNumber(usize);

        impl Dimension<Outline> for SectionNumber {
            fn zero() -> Self {
                SectionNumber(0)
            }
            fn add_summary(&mut self, s: &Outline) {
                self.0 += s.sections;
            }
        }

        let mut outline: SumTree<Section, 4> = SumTree::new();
        for s in 0..20u64 {
            outline.push(Section(tree_of(s * 10..s * 10 + 10)));
        }
        let before = outline.clone();

        // Add a word to section 7: its own tree grows, and the outline's
        // totals follow.
        let words =
            outline.update_at::<SectionNumber, _>(&SectionNumber(7), Bias::Right, |section| {
                section.0.push(9999);
                section.0.summary().count
            });
        assert_eq!(words, Some(11));
        assert_eq!(outline.summary().words.count, 201);
        assert_eq!(
            outline.summary().words.sum,
            before.summary().words.sum + 9999
        );
        outline.check().unwrap();
        assert_eq!(before.summary().words.count, 200, "the copy is untouched");
        assert_eq!(
            outline.update_at::<SectionNumber, _>(&SectionNumber(20), Bias::Right, |_| ()),
            None,
            "past the last section"
        );
    }
}
