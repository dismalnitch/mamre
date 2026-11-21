//! Cursor: walks a `SumTree`, keeping a running total in any `Dimension`, and
//! seeks to any `SeekTarget` for that dimension.
//!
//! A cursor is the read side of the tree. `seek` finds a position, and
//! `slice`/`suffix` cut the tree there. Together with `SumTree::append`, they
//! are all you need to build insert, delete and replace.

use std::cmp::Ordering;

use crate::{Dimension, Item, Node, SeekTarget, SumTree, units::CAP_SZ};

/// Which item a seek lands on when the target falls exactly on the boundary
/// between two items. `Left` stops at the end of the earlier item; `Right`
/// stops at the start of the later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// One level of the cursor's path from the root down to the current leaf.
struct StackEntry<'a, T: Item, const CAP: usize> {
    /// The node at this level.
    tree: &'a SumTree<T, CAP>,
    /// Which child (internal node) or item (leaf) the cursor is on.
    index: usize,
}

pub struct Cursor<'a, T: Item, D: Dimension<T::Summary>, const CAP: usize = CAP_SZ> {
    tree: &'a SumTree<T, CAP>,
    /// Root-to-leaf path. Empty means the cursor hasn't started yet.
    stack: Vec<StackEntry<'a, T, CAP>>,
    /// The total, in dimension `D`, of everything before the current item.
    position: D,
    /// True once the cursor has moved past the last item.
    at_end: bool,
}

impl<T: Item, const CAP: usize> SumTree<T, CAP> {
    /// A cursor over this tree that measures and seeks in dimension `D`. It
    /// starts before the first item; `seek` to move it.
    pub fn cursor<D: Dimension<T::Summary>>(&self) -> Cursor<'_, T, D, CAP> {
        Cursor::new(self)
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
        // TODO: look at the top of `stack`; if it's a leaf, return items[index].

        // if self.stack.first()?.tree.root.height() == 0 {
        //     self.stack.first()?.tree.root.items().get()
        // }
        todo!()
    }

    /// Moves to the next item.
    pub fn next(&mut self) {
        // TODO: advance the leaf index and add the item's summary to `position`.
        // When a leaf runs out, pop up until a node has another child, then
        // descend to that child's leftmost leaf. Set `at_end` if the root runs out.
        todo!()
    }

    /// Moves to the previous item.
    pub fn prev(&mut self) {
        // TODO: mirror of `next`. `position` has to go backwards, which
        // `Dimension` can't do (no subtraction). Either store each level's start
        // in `StackEntry` and recompute from there, or add subtraction to the trait.
        todo!()
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
            });
        }

        while let Some(entry) = self.stack.last_mut() {
            let tree: &'a SumTree<T, CAP> = entry.tree;
            let summaries: &'a [T::Summary] = match &*tree.root {
                Node::Internal {
                    child_summaries, ..
                } => child_summaries,
                Node::Leaf { item_summaries, .. } => item_summaries,
            };

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

    /// Returns everything from the cursor up to `end` as a new tree, leaving
    /// the cursor at `end`.
    pub fn slice(&mut self, end: &impl SeekTarget<T::Summary, D>, bias: Bias) -> SumTree<T, CAP> {
        // TODO: same walk as `seek_forward`, but collect what you pass. Whole
        // children you skip can be reused as-is (clone the `Arc`, no copying);
        // only the partial leaf at each edge needs new nodes. Build the result
        // with `SumTree::append` / `push`.
        let _ = (end, bias);
        todo!()
    }

    /// Returns everything from the cursor to the end as a new tree.
    pub fn suffix(&mut self) -> SumTree<T, CAP> {
        // TODO: no target to seek to, so walk instead: the rest of the current
        // leaf, then, going up the stack, every child to the right of the one
        // the cursor is in at that level. Like `slice`, reuse whole children
        // (clone the `Arc`) and `append` it all together.
        todo!()
    }
}

/// Whether a seek goes past an entry, given how the target compares with
/// the position at the entry's end.
fn passes(target_vs_end: Ordering, bias: Bias) -> bool {
    match bias {
        // Stop at the first entry whose end reaches the target.
        Bias::Left => target_vs_end == Ordering::Greater,
        // Stop at the first entry whose end is past the target.
        Bias::Right => target_vs_end != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    //! The cursor can't move yet; these check that the dimension and target
    //! traits fit together the way the cursor will use them.

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
    }
}
