//! Cursor: walks a `SumTree`, keeping a running total in any `Dimension`, and
//! seeks to any `SeekTarget` for that dimension.
//!
//! A cursor is the read side of the tree. `seek` finds a position, and
//! `slice`/`suffix` cut the tree there. Together with `SumTree::append`, they
//! are all you need to build insert, delete and replace.

use std::cmp::Ordering;

use crate::{Dimension, Item, Node, SeekTarget, SumTree, units::CAPSIZE};

/// Which item a seek lands on when the target falls exactly on the boundary
/// between two items. `Left` stops at the end of the earlier item; `Right`
/// stops at the start of the later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// One level of the cursor's path from the root down to the current leaf.
#[derive(Clone)]
struct StackEntry<'a, T: Item, D, const CAP: usize> {
    /// The node at this level.
    tree: &'a SumTree<T, CAP>,
    /// Which child (internal node) or item (leaf) the cursor is on.
    index: usize,
    /// The total, in dimension `D`, of everything before `tree`. Going back
    /// re-adds from here, since a dimension can't subtract.
    position: D,
}

#[derive(Clone)]
pub struct Cursor<'a, T: Item, D: Dimension<T::Summary>, const CAP: usize = CAPSIZE> {
    tree: &'a SumTree<T, CAP>,
    /// Root-to-leaf path. Empty means the cursor is before the first item
    /// (if not `at_end`) or past the last (if it is).
    stack: Vec<StackEntry<'a, T, D, CAP>>,
    /// The total, in dimension `D`, of everything before the current item.
    position: D,
    /// True once the cursor has moved past the last item.
    at_end: bool,
}

/// Every item of a tree, in order, by reference. The items borrow from the
/// tree, not from the iterator, so they can all be held at once.
///
/// A wrapper rather than `impl Iterator for Cursor`, because `Cursor` already
/// has its own `next` (which moves and returns nothing) and the two would
/// collide.
#[derive(Clone)]
pub struct Iter<'a, T: Item, const CAP: usize = CAPSIZE> {
    /// Tracks the `()` dimension: nothing to add up, so stepping is free.
    cursor: Cursor<'a, T, (), CAP>,
}

impl<'a, T: Item, const CAP: usize> Iterator for Iter<'a, T, CAP> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        // The cursor starts before the first item, so move, then look. Past
        // the end, `next` stays put and `item` keeps saying `None`.
        self.cursor.next();
        self.cursor.item()
    }
}

/// Once it returns `None`, it always will.
impl<T: Item, const CAP: usize> std::iter::FusedIterator for Iter<'_, T, CAP> {}

impl<'a, T: Item, const CAP: usize> IntoIterator for &'a SumTree<T, CAP> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T, CAP>;

    /// So `for item in &tree` works.
    fn into_iter(self) -> Iter<'a, T, CAP> {
        self.iter()
    }
}

impl<T: Item, const CAP: usize> SumTree<T, CAP> {
    /// A cursor over this tree that measures and seeks in dimension `D`. It
    /// starts before the first item; `seek` to move it.
    pub fn cursor<D: Dimension<T::Summary>>(&self) -> Cursor<'_, T, D, CAP> {
        Cursor::new(self)
    }

    /// Every item, in order, by reference.
    pub fn iter(&self) -> Iter<'_, T, CAP> {
        Iter {
            cursor: self.cursor(),
        }
    }
}

impl<'a, T: Item, D: Dimension<T::Summary>, const CAP: usize> Cursor<'a, T, D, CAP> {
    pub(crate) fn new(tree: &'a SumTree<T, CAP>) -> Self {
        Self {
            tree,
            stack: Vec::new(),
            position: D::zero(),
            at_end: false,
        }
    }

    /// Where the cursor is, measured in `D`: the total of everything before
    /// the current item.
    pub fn start(&self) -> &D {
        &self.position
    }

    /// The item under the cursor, or `None` before the start or past the end.
    pub fn item(&self) -> Option<&'a T> {
        if self.at_end {
            return None;
        }
        let entry = self.stack.last()?;
        let tree: &'a SumTree<T, CAP> = entry.tree;
        match &*tree.root {
            Node::Leaf { items, .. } => items.get(entry.index),
            Node::Internal { .. } => None,
        }
    }

    /// Moves to the next item. Before the start, that's the first item; on
    /// the last, it's past the end; past the end, it stays there.
    pub fn next(&mut self) {
        if self.at_end {
            return;
        }
        if self.stack.is_empty() {
            self.descend_front();
            if self.item().is_none() {
                // An empty tree: the first item is already past the end.
                self.stack.clear();
                self.at_end = true;
            }
            return;
        }

        // Step over the current item.
        let leaf = self.stack.last_mut().expect("on an item");
        self.position.add_summary(&entries(leaf.tree)[leaf.index]);
        leaf.index += 1;

        // Climb out of every node that's used up, into the next child up.
        while let Some(entry) = self.stack.last()
            && entry.index == entries(entry.tree).len()
        {
            self.stack.pop();
            match self.stack.last_mut() {
                Some(parent) => parent.index += 1,
                None => {
                    self.at_end = true;
                    return;
                }
            }
        }

        // Then down that child's left edge to its first item.
        self.descend_leftmost();
    }

    /// Moves to the previous item. On the first, that's before the start;
    /// past the end, it's the last item; before the start, it stays there.
    ///
    /// `position` is re-added from the nearest stored start, not subtracted.
    pub fn prev(&mut self) {
        if self.at_end {
            // Stand just past the root's last entry, so stepping back lands
            // on it like any other.
            self.at_end = false;
            self.stack.push(StackEntry {
                tree: self.tree,
                index: entries(self.tree).len(),
                position: D::zero(),
            });
        } else if self.stack.is_empty() {
            return;
        }

        // Climb to the lowest level with an entry before the current one.
        loop {
            match self.stack.last_mut() {
                Some(entry) if entry.index > 0 => {
                    entry.index -= 1;
                    break;
                }
                Some(_) => {
                    self.stack.pop();
                }
                None => {
                    // Stepped back off the first item: before the start.
                    self.position = D::zero();
                    return;
                }
            }
        }

        // That level's start, plus its entries before the new index.
        let entry = self.stack.last().expect("just stepped back in it");
        self.position = entry.position.clone();
        for s in &entries(entry.tree)[..entry.index] {
            self.position.add_summary(s);
        }

        // Then down that child's right edge to its last item.
        self.descend_rightmost();
    }

    /// Moves forward to the item `target` falls in, never back: a target
    /// behind the cursor leaves it where it is. Runs off the end if the target
    /// is past every item.
    ///
    /// At a boundary, `bias` picks the side: `Left` lands on the item ending
    /// there, `Right` on the item starting there, passing over empty items.
    ///
    /// Returns true if the target is exactly on the edge `bias` leans to: the
    /// landed item's end for `Left`, its start for `Right`.
    pub fn seek_forward(&mut self, target: &impl SeekTarget<T::Summary, D>, bias: Bias) -> bool {
        if self.stack.is_empty() && !self.at_end {
            self.stack.push(StackEntry {
                tree: self.tree,
                index: 0,
                position: D::zero(),
            });
        }

        while let Some(entry) = self.stack.last_mut() {
            let tree: &'a SumTree<T, CAP> = entry.tree;
            let summaries = entries(tree);

            // Skip whole entries that end before the target. For a child,
            // that's one summary add instead of a walk through it.
            while let Some(s) = summaries.get(entry.index) {
                let mut end = self.position.clone();
                end.add_summary(s);
                if !passes(target.cmp_position(&end), bias) {
                    break;
                }
                self.position = end;
                entry.index += 1;
            }

            let index = entry.index;
            if index == summaries.len() {
                // This node is used up, so carry on in its parent, after it.
                // `position` already counts everything in it.
                self.stack.pop();
                if let Some(parent) = self.stack.last_mut() {
                    parent.index += 1;
                }
                continue;
            }

            match &*tree.root {
                Node::Internal { children, .. } => {
                    // The target is in this child: go down into it.
                    self.stack.push(StackEntry {
                        tree: &children[index],
                        index: 0,
                        position: self.position.clone(),
                    });
                }
                Node::Leaf { .. } => {
                    // Landed on an item.
                    return match bias {
                        Bias::Left => {
                            let mut end = self.position.clone();
                            end.add_summary(&summaries[index]);
                            target.cmp_position(&end) == Ordering::Equal
                        }
                        Bias::Right => target.cmp_position(&self.position) == Ordering::Equal,
                    };
                }
            }
        }

        self.at_end = true;
        target.cmp_position(&self.position) == Ordering::Equal
    }

    /// Restarts from the beginning, then seeks to `target`. See `seek_forward`.
    pub fn seek(&mut self, target: &impl SeekTarget<T::Summary, D>, bias: Bias) -> bool {
        self.stack.clear();
        self.position = D::zero();
        self.at_end = false;
        self.seek_forward(target, bias)
    }

    /// Returns everything from the cursor up to where a seek for `end` lands
    /// (see `seek_forward`, including `bias`) as a new tree, and leaves the
    /// cursor there. A target behind the cursor gives an empty tree.
    ///
    /// A second cursor finds `end`, then the cut happens at the node where the
    /// two paths part: what's right of this cursor in its child, the whole
    /// children between, and what's left of `end` in its child. Whole subtrees
    /// are shared, not copied; only the leaves at the two edges are new.
    pub fn slice(&mut self, end: &impl SeekTarget<T::Summary, D>, bias: Bias) -> SumTree<T, CAP> {
        if self.at_end {
            return SumTree::new();
        }
        self.descend_front();
        let mut other = self.clone();
        other.seek_forward(end, bias);
        let slice = if other.at_end {
            self.suffix()
        } else {
            self.up_to(&other)
        };
        *self = other;
        slice
    }

    /// Returns everything from the cursor to the end as a new tree, and
    /// leaves the cursor past the end.
    pub fn suffix(&mut self) -> SumTree<T, CAP> {
        if self.at_end {
            return SumTree::new();
        }
        self.descend_front();
        let mut out = SumTree::new();
        append_right_of(&self.stack, &mut out);
        self.position.add_summary(out.summary());
        self.stack.clear();
        self.at_end = true;
        out
    }

    /// Fills in a fresh cursor's path, which is only implied by an empty
    /// stack: down the left edge, before the first item. Seeks carry on from
    /// it just as they would from the empty stack.
    fn descend_front(&mut self) {
        if !self.stack.is_empty() || self.at_end {
            return;
        }
        self.stack.push(StackEntry {
            tree: self.tree,
            index: 0,
            position: D::zero(),
        });
        self.descend_leftmost();
    }

    /// From the child the top of the stack points at, goes down left edges to
    /// a leaf, onto its first item. `position` is already before that child.
    fn descend_leftmost(&mut self) {
        loop {
            let entry = self.stack.last().expect("descending from a node");
            let tree: &'a SumTree<T, CAP> = entry.tree;
            let Node::Internal { children, .. } = &*tree.root else {
                return;
            };
            self.stack.push(StackEntry {
                tree: &children[entry.index],
                index: 0,
                position: self.position.clone(),
            });
        }
    }

    /// From the child the top of the stack points at, goes down right edges to
    /// a leaf, onto its last item, adding what it passes on the way.
    /// `position` is already before that child.
    fn descend_rightmost(&mut self) {
        loop {
            let entry = self.stack.last().expect("descending from a node");
            let tree: &'a SumTree<T, CAP> = entry.tree;
            let Node::Internal { children, .. } = &*tree.root else {
                return;
            };
            let child = &children[entry.index];
            let last = entries(child).len() - 1;
            self.stack.push(StackEntry {
                tree: child,
                index: last,
                position: self.position.clone(),
            });
            for s in &entries(child)[..last] {
                self.position.add_summary(s);
            }
        }
    }

    /// Everything from this cursor's item up to (not including) `other`'s.
    /// Both must be on an item, with `other` not behind.
    fn up_to(&self, other: &Self) -> SumTree<T, CAP> {
        let (a, b) = (&self.stack, &other.stack);
        debug_assert_eq!(a.len(), b.len(), "every leaf is at the same depth");

        // The paths share a top; the first level where they part is the
        // lowest node holding both. Same index all the way: same item.
        let Some(k) = (0..a.len()).find(|&k| a[k].index != b[k].index) else {
            return SumTree::new();
        };
        debug_assert!(std::ptr::eq(a[k].tree, b[k].tree));
        let (i, j) = (a[k].index, b[k].index);
        debug_assert!(i < j, "`other` is behind");

        match &*a[k].tree.root {
            Node::Leaf {
                items,
                item_summaries,
                ..
            } => SumTree::from_leaf(&items[i..j], &item_summaries[i..j]),
            Node::Internal { children, .. } => {
                let mut out = SumTree::new();
                append_right_of(&a[k + 1..], &mut out);
                for child in &children[i + 1..j] {
                    out.append(child.clone());
                }
                append_left_of(&b[k + 1..], &mut out);
                out
            }
        }
    }
}

/// Appends, in order, everything at or after the item `path` ends on: the
/// rest of its leaf, then, going up, every child right of the path.
fn append_right_of<T: Item, D, const CAP: usize>(
    path: &[StackEntry<'_, T, D, CAP>],
    out: &mut SumTree<T, CAP>,
) {
    for entry in path.iter().rev() {
        match &*entry.tree.root {
            Node::Leaf {
                items,
                item_summaries,
                ..
            } => out.append(SumTree::from_leaf(
                &items[entry.index..],
                &item_summaries[entry.index..],
            )),
            Node::Internal { children, .. } => {
                for child in &children[entry.index + 1..] {
                    out.append(child.clone());
                }
            }
        }
    }
}

/// Appends, in order, everything before the item `path` ends on: going down,
/// every child left of the path, then the start of its leaf.
fn append_left_of<T: Item, D, const CAP: usize>(
    path: &[StackEntry<'_, T, D, CAP>],
    out: &mut SumTree<T, CAP>,
) {
    for entry in path {
        match &*entry.tree.root {
            Node::Leaf {
                items,
                item_summaries,
                ..
            } => out.append(SumTree::from_leaf(
                &items[..entry.index],
                &item_summaries[..entry.index],
            )),
            Node::Internal { children, .. } => {
                for child in &children[..entry.index] {
                    out.append(child.clone());
                }
            }
        }
    }
}

/// The summaries of a node's entries: its children's, or its items'.
fn entries<T: Item, const CAP: usize>(tree: &SumTree<T, CAP>) -> &[T::Summary] {
    match &*tree.root {
        Node::Internal {
            child_summaries, ..
        } => child_summaries,
        Node::Leaf { item_summaries, .. } => item_summaries,
    }
}

/// Whether a seek goes past an entry, given how the target compares with
/// the position at the entry's end.
pub(crate) fn passes(target_vs_end: Ordering, bias: Bias) -> bool {
    match bias {
        // Stop at the first entry whose end reaches the target.
        Bias::Left => target_vs_end == Ordering::Greater,
        // Stop at the first entry whose end is past the target.
        Bias::Right => target_vs_end != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    //! The dimension and target traits fitting together, then seeks, slices
    //! and suffixes checked against a linear scan.

    use std::cmp::Ordering;

    use super::*;
    use crate::Summary;

    #[derive(Clone, Debug, PartialEq)]
    struct Totals {
        count: usize,
        len: u64,
    }

    impl Summary for Totals {
        fn empty() -> Self {
            Totals { count: 0, len: 0 }
        }
        fn concat(&mut self, other: &Self) {
            self.count += other.count;
            self.len += other.len;
        }
    }

    #[derive(Clone, Debug)]
    struct Word(u64);

    impl Item for Word {
        type Summary = Totals;
        fn summary(&self) -> Totals {
            Totals {
                count: 1,
                len: self.0,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Count(usize);

    impl Dimension<Totals> for Count {
        fn zero() -> Self {
            Count(0)
        }
        fn add_summary(&mut self, s: &Totals) {
            self.0 += s.count;
        }
    }

    /// A target that isn't a dimension at all: compares against the summary.
    struct Len(u64);

    impl SeekTarget<Totals, Totals> for Len {
        fn cmp_position(&self, position: &Totals) -> Ordering {
            self.0.cmp(&position.len)
        }
    }

    fn after_two() -> Totals {
        let mut t = Totals::empty();
        t.concat(&Word(3).summary());
        t.concat(&Word(4).summary());
        t
    }

    #[test]
    fn dimensions_track() {
        let s = after_two();

        let mut pair = <(Count, Totals) as Dimension<Totals>>::zero();
        pair.add_summary(&s);
        assert_eq!(pair.0, Count(2));
        assert_eq!(pair.1, s);

        <() as Dimension<Totals>>::zero();
        ().add_summary(&s);
    }

    #[test]
    fn targets_compare() {
        let s = after_two();
        let pair = (Count(2), s.clone());

        // An ordered dimension against itself.
        assert_eq!(
            SeekTarget::<Totals, Count>::cmp_position(&Count(1), &Count(2)),
            Ordering::Less
        );
        // The first of a pair against the pair.
        assert_eq!(
            SeekTarget::<Totals, (Count, Totals)>::cmp_position(&Count(2), &pair),
            Ordering::Equal
        );
        // A custom target against the summary.
        assert_eq!(Len(9).cmp_position(&s), Ordering::Greater);
    }

    #[test]
    fn cursors_take_any_dimension() {
        let tree: SumTree<Word> = SumTree::new();
        let _: Cursor<Word, (Count, Totals)> = tree.cursor();
        let _: Cursor<Word, Totals> = tree.cursor();
        let _: Cursor<Word, ()> = tree.cursor();
    }

    /// Where a seek by length should land, the slow way: the landed index,
    /// the length before it, and whether the target is exactly on the edge
    /// `bias` leans to.
    fn scan(lens: &[u64], target: u64, bias: Bias) -> (usize, u64, bool) {
        let mut start = 0;
        for (i, &len) in lens.iter().enumerate() {
            let end = start + len;
            let lands = match bias {
                Bias::Left => end >= target,
                Bias::Right => end > target,
            };
            if lands {
                let exact = match bias {
                    Bias::Left => end == target,
                    Bias::Right => start == target,
                };
                return (i, start, exact);
            }
            start = end;
        }
        (lens.len(), start, start == target)
    }

    proptest::proptest! {
        /// Lengths from 0, so there are empty items to pass over or land on,
        /// and tiny nodes, so the tree is several levels tall.
        #[test]
        fn seek_matches_a_scan(lens in proptest::collection::vec(0u64..4, 0..200)) {
            let mut tree: SumTree<Word, 4> = SumTree::new();
            for &len in &lens {
                tree.push(Word(len));
            }
            let total: u64 = lens.iter().sum();

            for bias in [Bias::Left, Bias::Right] {
                let mut forward = tree.cursor::<Totals>();
                for target in 0..=total + 2 {
                    let (index, before, exact) = scan(&lens, target, bias);

                    let mut fresh = tree.cursor::<Totals>();
                    let got = fresh.seek(&Len(target), bias);
                    proptest::prop_assert_eq!(
                        (fresh.start().count, fresh.start().len, got),
                        (index, before, exact),
                        "seek to {} {:?}", target, bias
                    );

                    // The same, reached by seeking forward from the last target.
                    let got = forward.seek_forward(&Len(target), bias);
                    proptest::prop_assert_eq!(
                        (forward.start().count, forward.start().len, got),
                        (index, before, exact),
                        "seek_forward to {} {:?}", target, bias
                    );
                }
            }
        }

        /// Cut a tree into three: seek to `from` (or don't, and start fresh),
        /// slice to `to`, then take the suffix. A `to` behind `from` slices
        /// nothing. Tiny nodes, so the two paths part at every level.
        #[test]
        fn slice_and_suffix_match_a_scan(
            lens in proptest::collection::vec(0u64..4, 0..200),
            from in 0u64..700,
            to in 0u64..700,
            fresh in proptest::bool::ANY,
            left in proptest::bool::ANY,
        ) {
            let bias = if left { Bias::Left } else { Bias::Right };
            let mut tree: SumTree<Word, 4> = SumTree::new();
            for &len in &lens {
                tree.push(Word(len));
            }
            let total: u64 = lens.iter().sum();
            let (from, to) = (from % (total + 3), to % (total + 3));
            let lens_of = |t: &SumTree<Word, 4>| t.items().iter().map(|w| w.0).collect::<Vec<_>>();

            let mut cursor = tree.cursor::<Totals>();
            let start = if fresh {
                0
            } else {
                cursor.seek(&Len(from), bias);
                scan(&lens, from, bias).0
            };
            let end = start.max(scan(&lens, to, bias).0);

            let slice = cursor.slice(&Len(to), bias);
            slice.check().map_err(proptest::test_runner::TestCaseError::fail)?;
            proptest::prop_assert_eq!(lens_of(&slice), &lens[start..end]);
            proptest::prop_assert_eq!(cursor.start().count, end);

            let rest = cursor.suffix();
            rest.check().map_err(proptest::test_runner::TestCaseError::fail)?;
            proptest::prop_assert_eq!(lens_of(&rest), &lens[end..]);
            proptest::prop_assert_eq!(cursor.start().count, lens.len());
            proptest::prop_assert_eq!(cursor.start().len, total);

            // The source tree is untouched.
            proptest::prop_assert_eq!(lens_of(&tree), lens);
        }

        /// Walk every item forward from before the start, off the end, then
        /// back again and off the front. Tiny nodes, so steps cross leaf and
        /// node edges at every level.
        #[test]
        fn next_and_prev_walk_every_item(lens in proptest::collection::vec(0u64..4, 0..200)) {
            let tree: SumTree<Word, 4> = tree_of(&lens);
            let before = prefix_sums(&lens);
            let n = lens.len();
            let mut cursor = tree.cursor::<Totals>();
            let at = |c: &Cursor<Word, Totals, 4>| {
                (c.item().map(|w| w.0), c.start().count, c.start().len)
            };

            proptest::prop_assert_eq!(at(&cursor), (None, 0, 0));
            for i in 0..n {
                cursor.next();
                proptest::prop_assert_eq!(at(&cursor), (Some(lens[i]), i, before[i]), "next to {}", i);
            }
            cursor.next();
            proptest::prop_assert_eq!(at(&cursor), (None, n, before[n]), "off the end");
            cursor.next();
            proptest::prop_assert_eq!(at(&cursor), (None, n, before[n]), "stays past the end");

            for i in (0..n).rev() {
                cursor.prev();
                proptest::prop_assert_eq!(at(&cursor), (Some(lens[i]), i, before[i]), "prev to {}", i);
            }
            cursor.prev();
            proptest::prop_assert_eq!(at(&cursor), (None, 0, 0), "off the front");
            cursor.prev();
            proptest::prop_assert_eq!(at(&cursor), (None, 0, 0), "stays before the start");

            // And forward again from there.
            cursor.next();
            proptest::prop_assert_eq!(at(&cursor), (lens.first().copied(), 0, 0));
        }

        /// Seek somewhere, then step both ways from there: steps and seeks
        /// keep the same path.
        #[test]
        fn steps_after_a_seek(
            lens in proptest::collection::vec(0u64..4, 1..200),
            target in 0u64..700,
            left in proptest::bool::ANY,
        ) {
            let bias = if left { Bias::Left } else { Bias::Right };
            let tree: SumTree<Word, 4> = tree_of(&lens);
            let before = prefix_sums(&lens);
            let total = before[lens.len()];
            let target = target % (total + 3);
            let i = scan(&lens, target, bias).0;

            let mut back = tree.cursor::<Totals>();
            back.seek(&Len(target), bias);
            back.prev();
            if i == 0 {
                proptest::prop_assert!(back.item().is_none());
                proptest::prop_assert_eq!(back.start().count, 0);
            } else {
                proptest::prop_assert_eq!(back.item().map(|w| w.0), Some(lens[i - 1]));
                proptest::prop_assert_eq!((back.start().count, back.start().len), (i - 1, before[i - 1]));
            }

            let mut on = tree.cursor::<Totals>();
            on.seek(&Len(target), bias);
            on.next();
            let j = (i + 1).min(lens.len());
            proptest::prop_assert_eq!(on.item().map(|w| w.0), lens.get(j).copied());
            proptest::prop_assert_eq!((on.start().count, on.start().len), (j, before[j]));
        }

        /// `iter` and `for item in &tree` give every item in order, then stop
        /// for good.
        #[test]
        fn iter_yields_every_item(lens in proptest::collection::vec(0u64..4, 0..200)) {
            let tree = tree_of(&lens);
            proptest::prop_assert_eq!(tree.iter().map(|w| w.0).collect::<Vec<_>>(), lens.clone());

            let mut seen = Vec::new();
            for word in &tree {
                seen.push(word.0);
            }
            proptest::prop_assert_eq!(&seen, &lens);

            let mut iter = tree.iter();
            for _ in &lens {
                iter.next();
            }
            proptest::prop_assert!(iter.next().is_none());
            proptest::prop_assert!(iter.next().is_none());
        }
    }

    /// Items borrow from the tree, not the iterator: they outlive it, and
    /// can all be held at once. This one is checked by compiling.
    #[test]
    fn iter_items_outlive_the_iterator() {
        let tree = tree_of(&[1, 2, 3]);
        let words: Vec<&Word> = {
            let iter = tree.iter();
            iter.collect()
        };
        assert_eq!(words.iter().map(|w| w.0).collect::<Vec<_>>(), [1, 2, 3]);
    }

    fn tree_of(lens: &[u64]) -> SumTree<Word, 4> {
        let mut tree = SumTree::new();
        for &len in lens {
            tree.push(Word(len));
        }
        tree
    }

    /// `before[i]` is the total length of the items before item `i`.
    fn prefix_sums(lens: &[u64]) -> Vec<u64> {
        let mut before = vec![0];
        for &len in lens {
            before.push(before.last().unwrap() + len);
        }
        before
    }
}
